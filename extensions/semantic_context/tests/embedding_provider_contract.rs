use std::{
    collections::VecDeque,
    future::Future,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    pin::pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
    thread,
    time::{Duration, Instant},
};

use semantic_context_extension::{
    EmbeddingProviderError, EmbeddingProviderPort, EmbeddingRequest, EmbeddingSpaceIdentity,
    IndexedEmbedding, OperationDeadline, PortFuture, SemanticOperation,
    adapters::embedding::{
        DeterministicEmbeddingFault, DeterministicEmbeddingProvider,
        DeterministicEmbeddingProviderConfig, EmbeddingProviderContractFixture,
        EmbeddingProviderContractHarness, OpenAiCompatibleEmbeddingProvider,
        ValidatedEmbeddingConfig,
    },
};

struct ContractServerState {
    faults: VecDeque<DeterministicEmbeddingFault>,
    stopped: bool,
}

struct ContractHttpServer {
    address: SocketAddr,
    state: Arc<Mutex<ContractServerState>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl ContractHttpServer {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind contract fixture");
        listener
            .set_nonblocking(true)
            .expect("make contract fixture nonblocking");
        let address = listener
            .local_addr()
            .expect("read contract fixture address");
        let state = Arc::new(Mutex::new(ContractServerState {
            faults: VecDeque::new(),
            stopped: false,
        }));
        let worker_state = Arc::clone(&state);
        let worker = thread::spawn(move || {
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if worker_state
                            .lock()
                            .expect("contract fixture state lock poisoned")
                            .stopped
                        {
                            break;
                        }
                        let state = Arc::clone(&worker_state);
                        thread::spawn(move || {
                            let input_count = read_input_count(&mut stream);
                            let fault = state
                                .lock()
                                .expect("contract fixture state lock poisoned")
                                .faults
                                .pop_front();
                            write_contract_response(&mut stream, input_count, fault);
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if worker_state
                            .lock()
                            .expect("contract fixture state lock poisoned")
                            .stopped
                        {
                            break;
                        }
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("contract fixture accept failed: {error}"),
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

    fn inject_fault(&self, fault: DeterministicEmbeddingFault) {
        self.state
            .lock()
            .expect("contract fixture state lock poisoned")
            .faults
            .push_back(fault);
    }
}

impl Drop for ContractHttpServer {
    fn drop(&mut self) {
        self.state
            .lock()
            .expect("contract fixture state lock poisoned")
            .stopped = true;
        let _ = TcpStream::connect(self.address);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("contract fixture worker panicked");
        }
    }
}

fn read_input_count(stream: &mut TcpStream) -> usize {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set contract fixture read timeout");
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 1_024];
    let header_end = loop {
        let read = stream.read(&mut buffer).expect("read contract request");
        assert!(read > 0, "client closed before sending complete headers");
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let head = String::from_utf8(bytes[..header_end].to_vec()).expect("request headers are UTF-8");
    let content_length = head
        .split("\r\n")
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length").then(|| {
                value
                    .trim()
                    .parse::<usize>()
                    .expect("numeric content length")
            })
        })
        .unwrap_or(0);
    while bytes.len() - header_end < content_length {
        let read = stream
            .read(&mut buffer)
            .expect("read contract request body");
        assert!(read > 0, "client closed before sending complete body");
        bytes.extend_from_slice(&buffer[..read]);
    }
    let body: serde_json::Value =
        serde_json::from_slice(&bytes[header_end..header_end + content_length])
            .expect("contract request body is JSON");
    body["input"]
        .as_array()
        .expect("request input is an array")
        .len()
}

fn write_contract_response(
    stream: &mut TcpStream,
    input_count: usize,
    fault: Option<DeterministicEmbeddingFault>,
) {
    let (status, body, delay) = match fault {
        Some(DeterministicEmbeddingFault::Unavailable) => {
            (503, "unavailable".to_owned(), Duration::ZERO)
        }
        Some(DeterministicEmbeddingFault::TimedOut) => {
            (200, success_body(input_count), Duration::from_millis(750))
        }
        Some(DeterministicEmbeddingFault::MalformedResponse) => {
            (200, "not-json".to_owned(), Duration::ZERO)
        }
        Some(DeterministicEmbeddingFault::NonFiniteVector { index }) => (
            200,
            format!(r#"{{"data":[{{"index":{index},"embedding":[1e39,2.0,3.0]}}]}}"#),
            Duration::ZERO,
        ),
        Some(DeterministicEmbeddingFault::DimensionMismatch { actual, .. }) => {
            let vector = vec!["1.0"; actual].join(",");
            (
                200,
                format!(
                    r#"{{"data":[{{"index":0,"embedding":[{vector}]}},{{"index":1,"embedding":[{vector}]}}]}}"#
                ),
                Duration::ZERO,
            )
        }
        None => (200, success_body(input_count), Duration::ZERO),
    };
    if !delay.is_zero() {
        thread::sleep(delay);
    }
    let reason = if status == 200 {
        "OK"
    } else {
        "Service Unavailable"
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

fn success_body(input_count: usize) -> String {
    let data = (0..input_count)
        .map(|index| format!(r#"{{"index":{index},"embedding":[1.0,2.0,3.0]}}"#))
        .collect::<Vec<_>>()
        .join(",");
    format!(r#"{{"data":[{data}]}}"#)
}

struct ObservedProvider {
    inner: OpenAiCompatibleEmbeddingProvider,
    deadlines: Mutex<Vec<OperationDeadline>>,
}

impl EmbeddingProviderPort for ObservedProvider {
    fn identity(&self) -> EmbeddingSpaceIdentity {
        self.inner.identity()
    }

    fn embed<'a>(
        &'a self,
        request: &'a EmbeddingRequest,
        deadline: OperationDeadline,
    ) -> PortFuture<'a, Result<Vec<IndexedEmbedding>, EmbeddingProviderError>> {
        self.deadlines
            .lock()
            .expect("contract deadline log lock poisoned")
            .push(deadline.clone());
        self.inner.embed(request, deadline)
    }
}

struct OpenAiContractFixture {
    server: ContractHttpServer,
    provider: ObservedProvider,
}

impl OpenAiContractFixture {
    fn new() -> Self {
        let server = ContractHttpServer::new();
        let provider = OpenAiCompatibleEmbeddingProvider::new(ValidatedEmbeddingConfig {
            base_url: server.base_url(),
            model: "contract-fixture".to_owned(),
            timeout_ms: 500,
            batch_size: 2,
            dimensions: Some(3),
            allow_remote: false,
            api_key_env: None,
        })
        .expect("a loopback OpenAI-compatible endpoint must be accepted");
        Self {
            server,
            provider: ObservedProvider {
                inner: provider,
                deadlines: Mutex::new(Vec::new()),
            },
        }
    }
}

impl EmbeddingProviderContractFixture for OpenAiContractFixture {
    fn provider(&self) -> &dyn EmbeddingProviderPort {
        &self.provider
    }

    fn inject_fault(&self, fault: DeterministicEmbeddingFault) {
        self.server.inject_fault(fault);
    }

    fn assert_all_calls_used_deadline(&self, expected: &OperationDeadline) {
        let deadlines = self
            .provider
            .deadlines
            .lock()
            .expect("contract deadline log lock poisoned");
        assert!(!deadlines.is_empty());
        for actual in deadlines.iter() {
            assert!(actual.deadline().is_some());
            assert_eq!(actual.operation(), expected.operation());
            assert_eq!(actual.layer(), expected.layer());
            assert_eq!(actual.budget_ms(), expected.budget_ms());
        }
        assert!(
            deadlines
                .windows(2)
                .all(|pair| pair[0].deadline() <= pair[1].deadline())
        );
    }

    fn deadline(&self) -> OperationDeadline {
        OperationDeadline::for_provider(
            Instant::now() + Duration::from_millis(500),
            SemanticOperation::Embedding,
            500,
        )
    }

    fn renew_deadline(&self, deadline: &OperationDeadline) -> OperationDeadline {
        OperationDeadline::for_provider(
            Instant::now() + Duration::from_millis(deadline.budget_ms()),
            deadline.operation().clone(),
            deadline.budget_ms(),
        )
    }
}

fn block_on<T>(future: impl Future<Output = T>) -> T {
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut future = pin!(future);

    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

#[test]
fn fake_embedding_provider_satisfies_shared_contract() {
    let provider = DeterministicEmbeddingProvider::new(DeterministicEmbeddingProviderConfig {
        identity: EmbeddingSpaceIdentity {
            provider: "deterministic".to_owned(),
            model: "contract-fixture".to_owned(),
            dimensions: Some(3),
        },
        dimensions: 3,
    });

    block_on(EmbeddingProviderContractHarness::new().verify(&provider));
}

#[test]
fn openai_compatible_provider_satisfies_shared_contract() {
    let fixture = OpenAiContractFixture::new();
    runtime().block_on(EmbeddingProviderContractHarness::new().verify(&fixture));
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("build contract test runtime")
}
