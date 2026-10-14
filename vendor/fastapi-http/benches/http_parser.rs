use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use fastapi_http::{Parser, QueryString, RequestLine};

// ============================================================================
// Test data: various HTTP requests of increasing complexity
// ============================================================================

fn simple_get() -> Vec<u8> {
    b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n".to_vec()
}

fn get_with_path_and_query() -> Vec<u8> {
    b"GET /api/v1/items/42?format=json&fields=id,name,price HTTP/1.1\r\n\
      Host: api.example.com\r\n\
      Accept: application/json\r\n\
      Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJ1c2VyIjoiMSJ9.abc123\r\n\
      \r\n"
        .to_vec()
}

fn post_with_json_body() -> Vec<u8> {
    let body = r#"{"name":"Widget","price":29.99,"description":"A fantastic widget","tags":["sale","new"]}"#;
    format!(
        "POST /api/v1/items HTTP/1.1\r\n\
         Host: api.example.com\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Accept: application/json\r\n\
         Authorization: Bearer token123\r\n\
         X-Request-Id: 550e8400-e29b-41d4-a716-446655440000\r\n\
         \r\n\
         {}",
        body.len(),
        body
    )
    .into_bytes()
}

fn request_with_many_headers(count: usize) -> Vec<u8> {
    let mut req = String::from("GET /resource HTTP/1.1\r\nHost: example.com\r\n");
    for i in 0..count {
        use std::fmt::Write;
        write!(req, "X-Custom-Header-{i}: value-{i}\r\n").unwrap();
    }
    req.push_str("\r\n");
    req.into_bytes()
}

fn large_query_string(param_count: usize) -> String {
    (0..param_count)
        .map(|i| format!("param{i}=value{i}"))
        .collect::<Vec<_>>()
        .join("&")
}

// ============================================================================
// Benchmarks: Request Line Parsing
// ============================================================================

fn bench_request_line_parsing(c: &mut Criterion) {
    let mut group = c.benchmark_group("request_line");

    let lines: Vec<(&str, &[u8])> = vec![
        ("minimal", b"GET / HTTP/1.1\r\n"),
        ("with_path", b"GET /api/v1/items/42 HTTP/1.1\r\n"),
        (
            "with_query",
            b"GET /search?q=rust+web+framework&page=1&limit=20 HTTP/1.1\r\n",
        ),
        ("post", b"POST /api/v1/items HTTP/1.1\r\n"),
        (
            "delete_with_path",
            b"DELETE /api/v1/items/42/comments/7 HTTP/1.1\r\n",
        ),
    ];

    for (name, line) in &lines {
        group.throughput(Throughput::Bytes(line.len() as u64));
        group.bench_with_input(BenchmarkId::new("parse", name), line, |b, line| {
            b.iter(|| RequestLine::parse(line).unwrap());
        });
    }

    group.finish();
}

// ============================================================================
// Benchmarks: Full Request Parsing
// ============================================================================

fn bench_full_request_parsing(c: &mut Criterion) {
    let mut group = c.benchmark_group("full_request");
    let parser = Parser::new();

    let requests: Vec<(&str, Vec<u8>)> = vec![
        ("simple_get", simple_get()),
        ("get_with_query", get_with_path_and_query()),
        ("post_json", post_with_json_body()),
        ("10_headers", request_with_many_headers(10)),
        ("30_headers", request_with_many_headers(30)),
        ("50_headers", request_with_many_headers(50)),
    ];

    for (name, req) in &requests {
        group.throughput(Throughput::Bytes(req.len() as u64));
        group.bench_with_input(BenchmarkId::new("parse", name), req, |b, req| {
            b.iter(|| parser.parse(req).unwrap());
        });
    }

    group.finish();
}

// ============================================================================
// Benchmarks: Query String Parsing
// ============================================================================

fn bench_query_string_parsing(c: &mut Criterion) {
    let mut group = c.benchmark_group("query_string");

    let queries: Vec<(&str, String)> = vec![
        ("single_param", "key=value".to_string()),
        ("3_params", "a=1&b=2&c=3".to_string()),
        ("10_params", large_query_string(10)),
        ("30_params", large_query_string(30)),
        (
            "percent_encoded",
            "name=hello%20world&q=%E4%B8%AD%E6%96%87".to_string(),
        ),
    ];

    for (name, qs_raw) in &queries {
        group.throughput(Throughput::Bytes(qs_raw.len() as u64));
        group.bench_with_input(BenchmarkId::new("parse", name), qs_raw, |b, qs_raw| {
            b.iter(|| {
                let qs = QueryString::parse(qs_raw);
                // Force iteration to materialize parsing
                qs.to_pairs()
            });
        });
    }

    // Benchmark single key lookup (common hot path)
    let qs_10 = large_query_string(10);
    group.bench_function("lookup_in_10_params", |b| {
        let qs = QueryString::parse(&qs_10);
        b.iter(|| qs.get("param5"));
    });

    group.finish();
}

// ============================================================================
// Benchmarks: Throughput (requests per second estimation)
// ============================================================================

fn bench_throughput_estimation(c: &mut Criterion) {
    let mut group = c.benchmark_group("throughput");
    let parser = Parser::new();

    // Simulate batch parsing to estimate RPS
    let simple = simple_get();
    let realistic = get_with_path_and_query();

    group.throughput(Throughput::Elements(100));
    group.bench_function("100_simple_gets", |b| {
        b.iter(|| {
            for _ in 0..100 {
                let _ = parser.parse(&simple).unwrap();
            }
        });
    });

    group.throughput(Throughput::Elements(100));
    group.bench_function("100_realistic_gets", |b| {
        b.iter(|| {
            for _ in 0..100 {
                let _ = parser.parse(&realistic).unwrap();
            }
        });
    });

    group.finish();
}

// ============================================================================
// Benchmarks: Header parsing isolation
// ============================================================================

fn bench_header_parsing(c: &mut Criterion) {
    use fastapi_http::HeadersParser;

    let mut group = c.benchmark_group("headers");

    let header_blocks: Vec<(&str, Vec<u8>)> = vec![
        (
            "2_headers",
            b"Host: example.com\r\nAccept: */*\r\n\r\n".to_vec(),
        ),
        (
            "typical_browser",
            b"Host: example.com\r\n\
              User-Agent: Mozilla/5.0\r\n\
              Accept: text/html,application/xhtml+xml\r\n\
              Accept-Language: en-US,en;q=0.9\r\n\
              Accept-Encoding: gzip, deflate, br\r\n\
              Connection: keep-alive\r\n\
              Cache-Control: no-cache\r\n\
              \r\n"
                .to_vec(),
        ),
    ];

    for (name, block) in &header_blocks {
        group.throughput(Throughput::Bytes(block.len() as u64));
        group.bench_with_input(BenchmarkId::new("parse", name), block, |b, block| {
            b.iter(|| HeadersParser::parse(block).unwrap());
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_request_line_parsing,
    bench_full_request_parsing,
    bench_query_string_parsing,
    bench_throughput_estimation,
    bench_header_parsing,
);
criterion_main!(benches);
