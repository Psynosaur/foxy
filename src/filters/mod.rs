// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Built-in filters
//!
//! Filters are **opt-in** – you must reference them in the `filters` array of
//! a `route` for them to execute.  Each filter is documented below together
//! with its configuration schema.

#[cfg(test)]
mod tests;

use std::cmp;
use std::sync::Arc;
use async_trait::async_trait;
use futures_util::{StreamExt, TryStreamExt};
use http_body_util::BodyExt;
use log::Level;
use crate::{error_fmt, warn_fmt, info_fmt, debug_fmt, trace_fmt};
use regex::Regex;
use serde::{Serialize, Deserialize};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Instant;
use tokio::sync::Mutex;
use std::io::{Read, Write};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use brotli;
use bytes::Bytes;
use dashmap::DashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_ENCODING, ACCEPT_ENCODING, CONTENT_TYPE};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde_json::Value;

use mime::Mime;
use prometheus::{Counter, Histogram, Registry, Opts, HistogramOpts};

use crate::core::{
    Filter, FilterType, ProxyRequest, ProxyResponse, ProxyError
};

/// Constructor signature every dynamic filter must implement
pub type FilterConstructor =
fn(serde_json::Value) -> Result<Arc<dyn Filter>, ProxyError>;


/// Global registry – `register_filter()` writes to it,
/// `FilterFactory::create_filter()` reads from it.
static FILTER_REGISTRY: Lazy<RwLock<HashMap<String, FilterConstructor>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// Register a filter under a unique name.
/// Call this **before** you build Foxy:
///
/// ```rust
/// use log::Level::Debug;
/// use foxy::{filters::register_filter, Filter};
///
/// #[derive(Debug)]
/// struct MyFilter;
/// impl MyFilter {
///     fn new(_cfg: serde_json::Value) -> Self { Self }
/// }
///
/// #[async_trait::async_trait]
/// impl foxy::Filter for MyFilter {
///     fn filter_type(&self) -> foxy::FilterType { foxy::FilterType::Pre }
///     fn name(&self) -> &str { "my_filter" }
/// }
///
/// register_filter("my_filter", |cfg| {
///     // turn `cfg` → your filter instance
///     Ok(std::sync::Arc::new(MyFilter::new(cfg)))
/// });
/// ```
pub fn register_filter(name: &str, ctor: FilterConstructor) {
    FILTER_REGISTRY
        .write()
        .expect("FILTER_REGISTRY poisoned")
        .insert(name.to_string(), ctor);
}

/// Internal helper – fetch a constructor if somebody registered one.
fn get_registered_filter(name: &str) -> Option<FilterConstructor> {
    FILTER_REGISTRY
        .read()
        .expect("FILTER_REGISTRY poisoned")
        .get(name)
        .copied()
}

/// Configuration for a logging filter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingFilterConfig {
    /// Whether to log request headers
    #[serde(default = "default_true")]
    pub log_request_headers: bool,

    /// Whether to log request body
    #[serde(default = "default_false")]
    pub log_request_body: bool,

    /// Whether to log response headers
    #[serde(default = "default_true")]
    pub log_response_headers: bool,

    /// Whether to log response body
    #[serde(default = "default_false")]
    pub log_response_body: bool,

    /// Log level to use
    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// Maximum body size to log (in bytes)
    #[serde(default = "default_max_body_size")]
    pub max_body_size: usize,
}

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}

fn default_log_level() -> String {
    "trace".to_string()
}

fn default_max_body_size() -> usize {
    1024 // Default to 1KB
}

impl Default for LoggingFilterConfig {
    fn default() -> Self {
        Self {
            log_request_headers: true,
            log_request_body: false,
            log_response_headers: true,
            log_response_body: false,
            log_level: "trace".to_string(),
            max_body_size: 1024,
        }
    }
}

/// A filter that logs HTTP requests and responses.
#[derive(Debug)]
pub struct LoggingFilter {
    config: LoggingFilterConfig,
}

impl Default for LoggingFilter {
    fn default() -> Self {
        Self::new(LoggingFilterConfig::default())
    }
}

impl LoggingFilter {
    /// Create a new logging filter with the given configuration.
    pub fn new(config: LoggingFilterConfig) -> Self {
        Self { config }
    }



    /// Get the log level from the configuration.
    fn get_log_level(&self) -> Level {
        match self.config.log_level.to_lowercase().as_str() {
            "error" => Level::Error,
            "warn" => Level::Warn,
            "info" => Level::Info,
            "debug" => Level::Debug,
            "trace" => Level::Trace,
            _ => Level::Trace,
        }
    }

    /// Log a message at the configured log level.
    fn log(&self, message: &str) {
        match self.get_log_level() {
            Level::Error => error_fmt!("LoggingFilter", "{}", message),
            Level::Warn => warn_fmt!("LoggingFilter", "{}", message),
            Level::Info => info_fmt!("LoggingFilter", "{}", message),
            Level::Debug => debug_fmt!("LoggingFilter", "{}", message),
            Level::Trace => trace_fmt!("LoggingFilter", "{}", message),
        }
    }

    /// Format headers for logging.
    fn format_headers(&self, headers: &reqwest::header::HeaderMap) -> String {
        let mut header_lines = Vec::new();
        for (name, value) in headers.iter() {
            if let Ok(value_str) = value.to_str() {
                header_lines.push(format!("{name}: {value_str}"));
            }
        }
        header_lines.join("\n")
    }

    /// Format body for logging (with size limits).
    fn format_body(&self, body: &[u8]) -> String {
        if body.is_empty() {
            return "[Empty body]".to_string();
        }

        let body_size = body.len();

        if body_size > self.config.max_body_size {
            return format!(
                "[Body truncated, showing {}/{} bytes]\n{}",
                self.config.max_body_size,
                body_size,
                String::from_utf8_lossy(&body[0..self.config.max_body_size])
            );
        }

        String::from_utf8_lossy(body).to_string()
    }
}

#[async_trait]
impl Filter for LoggingFilter {
    fn filter_type(&self) -> FilterType {
        FilterType::Both
    }

    fn name(&self) -> &str {
        "logging"
    }

    async fn pre_filter(&self, mut request: ProxyRequest) -> Result<ProxyRequest, ProxyError> {
        if self.config.log_request_headers {
            self.log(&format!(">> {} {}", request.method, request.path));
            let formatted_headers = self.format_headers(&request.headers);
            if !formatted_headers.is_empty() {
                for line in formatted_headers.lines() {
                    self.log(&format!(">> {line}"));
                }
            }
        }
        if self.config.log_request_body {
            let (new_body, snippet) = tee_body(request.body, self.config.max_body_size).await?;
            let formatted_body = self.format_body(snippet.as_bytes());
            self.log(&format!(">> Request Body:\n{formatted_body}"));
            request.body = new_body;
        }
        Ok(request)
    }

    async fn post_filter(
        &self,
        _req: ProxyRequest,
        mut response: ProxyResponse,
    ) -> Result<ProxyResponse, ProxyError> {
        if self.config.log_response_headers {
            self.log(&format!("<< {}", response.status));
            let formatted_headers = self.format_headers(&response.headers);
            if !formatted_headers.is_empty() {
                for line in formatted_headers.lines() {
                    self.log(&format!("<< {line}"));
                }
            }
        }
        if self.config.log_response_body {
            let (new_body, snippet) = tee_body(response.body, self.config.max_body_size).await?;
            let formatted_body = self.format_body(snippet.as_bytes());
            self.log(&format!("<< Response Body:\n{formatted_body}"));
            response.body = new_body;
        }
        Ok(response)
    }
}

async fn tee_body(
    body: reqwest::Body,
    limit: usize,
) -> Result<(reqwest::Body, String), ProxyError> {
    // Turn the body into a stream of Bytes
    let mut stream_in = body.into_data_stream();
    
    // Create a buffer to capture the first `limit` bytes
    let mut captured = Vec::<u8>::with_capacity(limit);
    
    // Create a vector to collect chunks for replay
    let mut chunks = Vec::new();
    
    // Read chunks until we have enough bytes or reach EOF
    while captured.len() < limit {
        match stream_in.next().await {
            Some(Ok(chunk)) => {
                // Store the chunk for replay
                let chunk_clone = chunk.clone();
                chunks.push(Ok(chunk));
                
                // Capture bytes up to the limit
                if captured.len() < limit {
                    let remaining = limit - captured.len();
                    let take = cmp::min(remaining, chunk_clone.len());
                    captured.extend_from_slice(&chunk_clone[..take]);
                }
            }
            Some(Err(e)) => return Err(ProxyError::Other(e.to_string())),
            None => break, // EOF
        }
    }
    
    // Create a stream that yields our buffered chunks followed by any remaining chunks
    let combined_stream = futures_util::stream::iter(chunks)
        .chain(stream_in.map_err(std::io::Error::other));
    
    // Wrap the stream back into a reqwest::Body
    let new_body = reqwest::Body::wrap_stream(combined_stream);
    
    // Convert captured bytes to string
    let snippet = String::from_utf8_lossy(&captured).to_string();
    
    Ok((new_body, snippet))
}

/// Configuration for a header modification filter.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HeaderFilterConfig {
    /// Headers to add or replace in the request
    #[serde(default)]
    pub add_request_headers: std::collections::HashMap<String, String>,

    /// Headers to remove from the request
    #[serde(default)]
    pub remove_request_headers: Vec<String>,

    /// Headers to add or replace in the response
    #[serde(default)]
    pub add_response_headers: std::collections::HashMap<String, String>,

    /// Headers to remove from the response
    #[serde(default)]
    pub remove_response_headers: Vec<String>,
}



/// A filter that modifies HTTP headers.
#[derive(Debug)]
pub struct HeaderFilter {
    config: HeaderFilterConfig,
}

impl Default for HeaderFilter {
    fn default() -> Self {
        Self::new(HeaderFilterConfig::default())
    }
}

impl HeaderFilter {
    /// Create a new header filter with the given configuration.
    pub fn new(config: HeaderFilterConfig) -> Self {
        Self { config }
    }



    /// Apply header modifications to the given header map.
    fn apply_headers(&self, headers: &mut reqwest::header::HeaderMap,
                     add_headers: &std::collections::HashMap<String, String>,
                     remove_headers: &[String]) {
        // Remove headers
        for header_name in remove_headers {
            if let Ok(name) = reqwest::header::HeaderName::from_bytes(header_name.as_bytes()) {
                headers.remove(&name);
            }
        }

        // Add or replace headers
        for (name, value) in add_headers {
            if let (Ok(header_name), Ok(header_value)) = (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                reqwest::header::HeaderValue::from_str(value)
            ) {
                headers.insert(header_name, header_value);
            }
        }
    }
}

#[async_trait]
impl Filter for HeaderFilter {
    fn filter_type(&self) -> FilterType {
        FilterType::Both
    }

    fn name(&self) -> &str {
        "header"
    }

    async fn pre_filter(&self, mut request: ProxyRequest) -> Result<ProxyRequest, ProxyError> {
        self.apply_headers(
            &mut request.headers,
            &self.config.add_request_headers,
            &self.config.remove_request_headers
        );

        Ok(request)
    }

    async fn post_filter(&self, _request: ProxyRequest, mut response: ProxyResponse) -> Result<ProxyResponse, ProxyError> {
        self.apply_headers(
            &mut response.headers,
            &self.config.add_response_headers,
            &self.config.remove_response_headers
        );

        Ok(response)
    }
}

/// Configuration for a timeout filter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeoutFilterConfig {
    /// Timeout in milliseconds
    pub timeout_ms: u64,
}

impl Default for TimeoutFilterConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 30000, // 30 seconds
        }
    }
}

/// A filter that enforces request timeouts.
#[derive(Debug)]
pub struct TimeoutFilter {
    config: TimeoutFilterConfig,
}

impl Default for TimeoutFilter {
    fn default() -> Self {
        Self::new(TimeoutFilterConfig::default())
    }
}

impl TimeoutFilter {
    /// Create a new timeout filter with the given configuration.
    pub fn new(config: TimeoutFilterConfig) -> Self {
        Self { config }
    }


}

#[async_trait]
impl Filter for TimeoutFilter {
    fn filter_type(&self) -> FilterType {
        FilterType::Pre
    }

    fn name(&self) -> &str {
        "timeout"
    }

    async fn pre_filter(&self, request: ProxyRequest) -> Result<ProxyRequest, ProxyError> {
        // Store the timeout in the request context
        {
            let mut context = request.context.write().await;
            context.attributes.insert(
                "timeout_ms".to_string(),
                serde_json::to_value(self.config.timeout_ms).unwrap()
            );
        }

        Ok(request)
    }
}

/// Configuration for a path rewrite filter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathRewriteFilterConfig {
    /// The pattern to match (regex)
    pub pattern: String,
    /// The replacement pattern
    pub replacement: String,
    /// Whether to apply on the request path
    #[serde(default = "default_true")]
    pub rewrite_request: bool,
    /// Whether to apply on the response path (if found in headers or body)
    #[serde(default = "default_false")]
    pub rewrite_response: bool,
}

/// A filter that rewrites request and response paths based on regex patterns.
#[derive(Debug)]
pub struct PathRewriteFilter {
    /// The configuration for this filter
    config: PathRewriteFilterConfig,
    /// Compiled regex for path matching
    regex: Regex,
}

impl PathRewriteFilter {
    /// Create a new path rewrite filter with the given configuration.
    pub fn new(config: PathRewriteFilterConfig) -> Result<Self, ProxyError> {
        // Compile the regex
        let regex = Regex::new(&config.pattern)
            .map_err(|e| {
                let err = ProxyError::FilterError(format!("Invalid regex pattern '{}': {}", config.pattern, e));
                error_fmt!("PathRewriteFilter", "{}", err);
                err
            })?;

        Ok(Self { config, regex })
    }

    /// Create a new path rewrite filter with default configuration.
    pub fn with_defaults() -> Result<Self, ProxyError> {
        Self::new(PathRewriteFilterConfig {
            pattern: "(.*)".to_string(),
            replacement: "$1".to_string(),
            rewrite_request: true,
            rewrite_response: false,
        })
    }
}

#[async_trait]
impl Filter for PathRewriteFilter {
    fn filter_type(&self) -> FilterType {
        FilterType::Both
    }

    fn name(&self) -> &str {
        "path_rewrite"
    }

    async fn pre_filter(&self, mut request: ProxyRequest) -> Result<ProxyRequest, ProxyError> {
        if self.config.rewrite_request {
            // Apply path rewriting on the request path
            let original_path = request.path.clone();
            let rewritten_path = self.regex.replace_all(&request.path, &self.config.replacement).to_string();

            if rewritten_path != original_path {
                debug_fmt!("PathRewriteFilter", "Rewriting path from {} to {}", original_path, rewritten_path);
                request.path = rewritten_path;
            } else {
                trace_fmt!("PathRewriteFilter", "Path rewrite pattern matched but did not change path: {}", original_path);
            }
        }

        Ok(request)
    }

    async fn post_filter(&self, _request: ProxyRequest, response: ProxyResponse) -> Result<ProxyResponse, ProxyError> {
        if self.config.rewrite_response {
            debug_fmt!("PathRewriteFilter", "Response path rewriting is configured but not implemented yet");
            // TODO: Implement response path rewriting when needed
            // This would require parsing and modifying the response body
            // which is complex and content-type dependent
        }

        Ok(response)
    }
}

/// Configuration for a rate limiting filter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitFilterConfig {
    /// Number of requests allowed per second
    pub requests_per_second: f64,
    /// Maximum burst size (number of requests that can be made immediately)
    pub burst_size: u32,
}

impl Default for RateLimitFilterConfig {
    fn default() -> Self {
        Self {
            requests_per_second: 10.0,
            burst_size: 10,
        }
    }
}

/// A token bucket for rate limiting
#[derive(Debug)]
struct TokenBucket {
    /// Maximum number of tokens (burst size)
    capacity: u32,
    /// Current number of tokens
    tokens: f64,
    /// Rate at which tokens are replenished (tokens per second)
    refill_rate: f64,
    /// Last time tokens were replenished
    last_refill: Instant,
}

impl TokenBucket {
    fn new(capacity: u32, refill_rate: f64) -> Self {
        Self {
            capacity,
            tokens: capacity as f64,
            refill_rate,
            last_refill: Instant::now(),
        }
    }

    /// Try to consume a token. Returns true if successful, false if rate limited.
    fn try_consume(&mut self) -> bool {
        self.refill();

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Refill tokens based on elapsed time
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();

        if elapsed > 0.0 {
            let tokens_to_add = elapsed * self.refill_rate;
            self.tokens = (self.tokens + tokens_to_add).min(self.capacity as f64);
            self.last_refill = now;
        }
    }
}

/// A filter that implements rate limiting using a token bucket algorithm.
#[derive(Debug)]
pub struct RateLimitFilter {
    config: RateLimitFilterConfig,
    /// Token bucket for rate limiting (shared across all requests)
    bucket: Arc<Mutex<TokenBucket>>,
}

impl RateLimitFilter {
    /// Create a new rate limit filter with the given configuration.
    pub fn new(config: RateLimitFilterConfig) -> Self {
        let bucket = TokenBucket::new(config.burst_size, config.requests_per_second);
        Self {
            config,
            bucket: Arc::new(Mutex::new(bucket)),
        }
    }
}

#[async_trait]
impl Filter for RateLimitFilter {
    fn filter_type(&self) -> FilterType {
        FilterType::Pre
    }

    fn name(&self) -> &str {
        "rate_limit"
    }

    async fn pre_filter(&self, request: ProxyRequest) -> Result<ProxyRequest, ProxyError> {
        let mut bucket = self.bucket.lock().await;

        if bucket.try_consume() {
            // Request allowed
            debug_fmt!("RateLimitFilter", "Request allowed for path: {}", request.path);
            Ok(request)
        } else {
            // Rate limit exceeded
            warn_fmt!("RateLimitFilter", "Rate limit exceeded for path: {}", request.path);
            Err(ProxyError::RateLimitExceeded(format!(
                "Rate limit exceeded: {} requests per second, burst size: {}",
                self.config.requests_per_second,
                self.config.burst_size
            )))
        }
    }
}

/// Configuration for a compression filter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressionFilterConfig {
    /// Enable gzip compression
    #[serde(default = "default_true")]
    pub enable_gzip: bool,
    /// Enable brotli compression
    #[serde(default = "default_false")]
    pub enable_br: bool,
    /// Minimum response size to compress (in bytes)
    #[serde(default = "default_min_compress_size")]
    pub min_compress_size: usize,
    /// Maximum response size to compress (in bytes)
    #[serde(default = "default_max_compress_size")]
    pub max_compress_size: usize,
    /// Compression level (1-9 for gzip, 1-11 for brotli)
    #[serde(default = "default_compression_level")]
    pub compression_level: u32,
}

fn default_min_compress_size() -> usize {
    1024 // 1KB
}

fn default_max_compress_size() -> usize {
    10 * 1024 * 1024 // 10MB
}

fn default_compression_level() -> u32 {
    6
}

impl Default for CompressionFilterConfig {
    fn default() -> Self {
        Self {
            enable_gzip: true,
            enable_br: false,
            min_compress_size: default_min_compress_size(),
            max_compress_size: default_max_compress_size(),
            compression_level: default_compression_level(),
        }
    }
}

/// A filter that compresses responses and decompresses requests.
#[derive(Debug)]
pub struct CompressionFilter {
    config: CompressionFilterConfig,
}

impl CompressionFilter {
    /// Create a new compression filter with the given configuration.
    pub fn new(config: CompressionFilterConfig) -> Self {
        Self { config }
    }

    /// Check if content type is compressible
    fn is_compressible_content_type(&self, content_type: Option<&HeaderValue>) -> bool {
        if let Some(ct) = content_type {
            if let Ok(ct_str) = ct.to_str() {
                if let Ok(mime_type) = ct_str.parse::<Mime>() {
                    return match mime_type.type_() {
                        mime::TEXT => true,
                        mime::APPLICATION => {
                            matches!(mime_type.subtype().as_str(),
                                "json" | "javascript" | "xml" | "xhtml+xml" | "rss+xml" | "atom+xml")
                        }
                        _ => false,
                    };
                }
            }
        }
        false
    }

    /// Compress data using gzip
    fn compress_gzip(&self, data: &[u8]) -> Result<Vec<u8>, ProxyError> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::new(self.config.compression_level));
        encoder.write_all(data)
            .map_err(|e| ProxyError::FilterError(format!("Gzip compression failed: {}", e)))?;
        encoder.finish()
            .map_err(|e| ProxyError::FilterError(format!("Gzip compression failed: {}", e)))
    }

    /// Compress data using brotli
    fn compress_brotli(&self, data: &[u8]) -> Result<Vec<u8>, ProxyError> {
        let mut output = Vec::new();
        let mut compressor = brotli::CompressorWriter::new(&mut output, 4096, self.config.compression_level, 22);
        compressor.write_all(data)
            .map_err(|e| ProxyError::FilterError(format!("Brotli compression failed: {}", e)))?;
        compressor.flush()
            .map_err(|e| ProxyError::FilterError(format!("Brotli compression failed: {}", e)))?;
        drop(compressor);
        Ok(output)
    }

    /// Decompress gzip data
    fn decompress_gzip(&self, data: &[u8]) -> Result<Vec<u8>, ProxyError> {
        let mut decoder = GzDecoder::new(data);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed)
            .map_err(|e| ProxyError::FilterError(format!("Gzip decompression failed: {}", e)))?;
        Ok(decompressed)
    }

    /// Decompress brotli data
    fn decompress_brotli(&self, data: &[u8]) -> Result<Vec<u8>, ProxyError> {
        let mut decoder = brotli::DecompressorWriter::new(Vec::new(), 4096);
        decoder.write_all(data)
            .map_err(|e| ProxyError::FilterError(format!("Brotli decompression failed: {}", e)))?;
        decoder.flush()
            .map_err(|e| ProxyError::FilterError(format!("Brotli decompression failed: {}", e)))?;
        decoder.into_inner()
            .map_err(|_| ProxyError::FilterError("Brotli decompression failed: could not get inner buffer".to_string()))
    }
}

#[async_trait]
impl Filter for CompressionFilter {
    fn filter_type(&self) -> FilterType {
        FilterType::Both
    }

    fn name(&self) -> &str {
        "compression"
    }

    async fn pre_filter(&self, mut request: ProxyRequest) -> Result<ProxyRequest, ProxyError> {
        // Decompress request body if it's compressed
        if let Some(encoding) = request.headers.get(CONTENT_ENCODING) {
            if let Ok(encoding_str) = encoding.to_str() {
                match encoding_str {
                    "gzip" => {
                        let (new_body, data) = tee_body(request.body, self.config.max_compress_size).await?;
                        if !data.is_empty() {
                            let decompressed = self.decompress_gzip(data.as_bytes())?;
                            request.body = reqwest::Body::from(decompressed);
                            request.headers.remove(CONTENT_ENCODING);
                            debug_fmt!("CompressionFilter", "Decompressed gzip request body");
                        } else {
                            request.body = new_body;
                        }
                    }
                    "br" => {
                        let (new_body, data) = tee_body(request.body, self.config.max_compress_size).await?;
                        if !data.is_empty() {
                            let decompressed = self.decompress_brotli(data.as_bytes())?;
                            request.body = reqwest::Body::from(decompressed);
                            request.headers.remove(CONTENT_ENCODING);
                            debug_fmt!("CompressionFilter", "Decompressed brotli request body");
                        } else {
                            request.body = new_body;
                        }
                    }
                    _ => {
                        debug_fmt!("CompressionFilter", "Unsupported compression encoding: {}", encoding_str);
                    }
                }
            }
        }

        Ok(request)
    }

    async fn post_filter(&self, request: ProxyRequest, mut response: ProxyResponse) -> Result<ProxyResponse, ProxyError> {
        // Only compress if client accepts compression
        let accept_encoding = request.headers.get(ACCEPT_ENCODING)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");

        // Check if response is already compressed
        if response.headers.get(CONTENT_ENCODING).is_some() {
            debug_fmt!("CompressionFilter", "Response already compressed, skipping");
            return Ok(response);
        }

        // Check if content type is compressible
        if !self.is_compressible_content_type(response.headers.get(CONTENT_TYPE)) {
            debug_fmt!("CompressionFilter", "Content type not compressible, skipping");
            return Ok(response);
        }

        // Read response body
        let (new_body, data) = tee_body(response.body, self.config.max_compress_size).await?;

        if data.len() < self.config.min_compress_size {
            debug_fmt!("CompressionFilter", "Response too small to compress: {} bytes", data.len());
            response.body = new_body;
            return Ok(response);
        }

        if data.len() > self.config.max_compress_size {
            debug_fmt!("CompressionFilter", "Response too large to compress: {} bytes", data.len());
            response.body = new_body;
            return Ok(response);
        }

        // Choose compression method based on client support and configuration
        let compressed_data = if self.config.enable_br && accept_encoding.contains("br") {
            let compressed = self.compress_brotli(data.as_bytes())?;
            response.headers.insert(CONTENT_ENCODING, HeaderValue::from_static("br"));
            debug_fmt!("CompressionFilter", "Compressed response with brotli: {} -> {} bytes",
                data.len(), compressed.len());
            compressed
        } else if self.config.enable_gzip && accept_encoding.contains("gzip") {
            let compressed = self.compress_gzip(data.as_bytes())?;
            response.headers.insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
            debug_fmt!("CompressionFilter", "Compressed response with gzip: {} -> {} bytes",
                data.len(), compressed.len());
            compressed
        } else {
            debug_fmt!("CompressionFilter", "Client doesn't support compression or compression disabled");
            response.body = new_body;
            return Ok(response);
        };

        // Update content length
        response.headers.insert("content-length", HeaderValue::from_str(&compressed_data.len().to_string())
            .map_err(|e| ProxyError::FilterError(format!("Failed to set content-length: {}", e)))?);

        response.body = reqwest::Body::from(compressed_data);
        Ok(response)
    }
}

/// Configuration for a retry filter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryFilterConfig {
    /// Maximum number of retries
    #[serde(default = "default_retries")]
    pub retries: u32,
    /// Initial backoff delay in milliseconds
    #[serde(default = "default_backoff_ms")]
    pub backoff_ms: u64,
    /// Maximum backoff delay in milliseconds
    #[serde(default = "default_max_backoff_ms")]
    pub max_backoff_ms: u64,
    /// Backoff multiplier for exponential backoff
    #[serde(default = "default_backoff_multiplier")]
    pub backoff_multiplier: f64,
    /// Whether to retry on 5xx status codes
    #[serde(default = "default_true")]
    pub retry_on_5xx: bool,
    /// Whether to retry on network errors
    #[serde(default = "default_true")]
    pub retry_on_network_error: bool,
}

fn default_retries() -> u32 {
    3
}

fn default_backoff_ms() -> u64 {
    100
}

fn default_max_backoff_ms() -> u64 {
    30000 // 30 seconds
}

fn default_backoff_multiplier() -> f64 {
    2.0
}

impl Default for RetryFilterConfig {
    fn default() -> Self {
        Self {
            retries: default_retries(),
            backoff_ms: default_backoff_ms(),
            max_backoff_ms: default_max_backoff_ms(),
            backoff_multiplier: default_backoff_multiplier(),
            retry_on_5xx: true,
            retry_on_network_error: true,
        }
    }
}

/// A filter that retries failed requests with exponential backoff.
#[derive(Debug)]
pub struct RetryFilter {
    config: RetryFilterConfig,
}

impl RetryFilter {
    /// Create a new retry filter with the given configuration.
    pub fn new(config: RetryFilterConfig) -> Self {
        Self { config }
    }

    /// Check if an error should trigger a retry
    fn should_retry(&self, error: &ProxyError) -> bool {
        match error {
            ProxyError::ClientError(reqwest_error) => {
                if self.config.retry_on_network_error && reqwest_error.is_connect() {
                    return true;
                }
                if self.config.retry_on_network_error && reqwest_error.is_timeout() {
                    return true;
                }
                if self.config.retry_on_5xx {
                    if let Some(status) = reqwest_error.status() {
                        return status.as_u16() >= 500;
                    }
                }
                false
            }
            ProxyError::Timeout(_) => self.config.retry_on_network_error,
            _ => false,
        }
    }

    /// Check if a status code should trigger a retry
    fn should_retry_status(&self, status: u16) -> bool {
        self.config.retry_on_5xx && status >= 500
    }

    /// Calculate backoff delay for the given attempt
    fn calculate_backoff(&self, attempt: u32) -> Duration {
        let delay_ms = (self.config.backoff_ms as f64 * self.config.backoff_multiplier.powi(attempt as i32)) as u64;
        Duration::from_millis(delay_ms.min(self.config.max_backoff_ms))
    }
}

#[async_trait]
impl Filter for RetryFilter {
    fn filter_type(&self) -> FilterType {
        FilterType::Pre
    }

    fn name(&self) -> &str {
        "retry"
    }

    async fn pre_filter(&self, request: ProxyRequest) -> Result<ProxyRequest, ProxyError> {
        // Store retry metadata in request context
        {
            let mut context = request.context.write().await;
            context.attributes.insert("retry_attempts".to_string(), serde_json::Value::Number(0.into()));
            context.attributes.insert("retry_config".to_string(), serde_json::to_value(&self.config)
                .map_err(|e| ProxyError::FilterError(format!("Failed to serialize retry config: {}", e)))?);
        }

        debug_fmt!("RetryFilter", "Configured retry for request: {} {}", request.method, request.path);
        Ok(request)
    }
}

/// Circuit breaker states
#[derive(Debug, Clone, PartialEq, Eq)]
enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

/// Circuit breaker statistics
#[derive(Debug, Clone)]
struct CircuitStats {
    failure_count: u32,
    success_count: u32,
    last_failure_time: Option<Instant>,
    state: CircuitState,
}

impl Default for CircuitStats {
    fn default() -> Self {
        Self {
            failure_count: 0,
            success_count: 0,
            last_failure_time: None,
            state: CircuitState::Closed,
        }
    }
}

/// Configuration for a circuit breaker filter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreakerFilterConfig {
    /// Number of failures before opening the circuit
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: u32,
    /// Time to wait before attempting to close the circuit (in milliseconds)
    #[serde(default = "default_reset_timeout_ms")]
    pub reset_timeout_ms: u64,
    /// Number of successful requests needed to close the circuit from half-open state
    #[serde(default = "default_success_threshold")]
    pub success_threshold: u32,
    /// Whether to consider 5xx status codes as failures
    #[serde(default = "default_true")]
    pub fail_on_5xx: bool,
    /// Whether to consider network errors as failures
    #[serde(default = "default_true")]
    pub fail_on_network_error: bool,
}

fn default_failure_threshold() -> u32 {
    5
}

fn default_reset_timeout_ms() -> u64 {
    60000 // 1 minute
}

fn default_success_threshold() -> u32 {
    3
}

impl Default for CircuitBreakerFilterConfig {
    fn default() -> Self {
        Self {
            failure_threshold: default_failure_threshold(),
            reset_timeout_ms: default_reset_timeout_ms(),
            success_threshold: default_success_threshold(),
            fail_on_5xx: true,
            fail_on_network_error: true,
        }
    }
}

/// A filter that implements circuit breaker pattern to prevent cascading failures.
#[derive(Debug)]
pub struct CircuitBreakerFilter {
    config: CircuitBreakerFilterConfig,
    /// Circuit breaker statistics per target
    stats: Arc<DashMap<String, CircuitStats>>,
}

impl CircuitBreakerFilter {
    /// Create a new circuit breaker filter with the given configuration.
    pub fn new(config: CircuitBreakerFilterConfig) -> Self {
        Self {
            config,
            stats: Arc::new(DashMap::new()),
        }
    }

    /// Get the target key for circuit breaker tracking
    fn get_target_key(&self, request: &ProxyRequest) -> String {
        request.custom_target.as_ref()
            .map(|t| t.clone())
            .unwrap_or_else(|| format!("{}:{}", request.path, request.method))
    }

    /// Check if the circuit should be opened based on current stats
    fn should_open_circuit(&self, stats: &CircuitStats) -> bool {
        stats.failure_count >= self.config.failure_threshold
    }

    /// Check if the circuit should transition from open to half-open
    fn should_try_half_open(&self, stats: &CircuitStats) -> bool {
        if let Some(last_failure) = stats.last_failure_time {
            let reset_timeout = Duration::from_millis(self.config.reset_timeout_ms);
            Instant::now().duration_since(last_failure) >= reset_timeout
        } else {
            false
        }
    }

    /// Check if the circuit should close from half-open state
    fn should_close_circuit(&self, stats: &CircuitStats) -> bool {
        stats.success_count >= self.config.success_threshold
    }

    /// Record a successful request
    fn record_success(&self, target_key: &str) {
        let mut stats = self.stats.entry(target_key.to_string()).or_default();

        match stats.state {
            CircuitState::Closed => {
                // Reset failure count on success
                stats.failure_count = 0;
            }
            CircuitState::HalfOpen => {
                stats.success_count += 1;
                if self.should_close_circuit(&stats) {
                    stats.state = CircuitState::Closed;
                    stats.failure_count = 0;
                    stats.success_count = 0;
                    info_fmt!("CircuitBreakerFilter", "Circuit closed for target: {}", target_key);
                }
            }
            CircuitState::Open => {
                // Should not happen, but reset if it does
                stats.state = CircuitState::Closed;
                stats.failure_count = 0;
                stats.success_count = 0;
            }
        }
    }

    /// Record a failed request
    fn record_failure(&self, target_key: &str) {
        let mut stats = self.stats.entry(target_key.to_string()).or_default();

        match stats.state {
            CircuitState::Closed => {
                stats.failure_count += 1;
                stats.last_failure_time = Some(Instant::now());

                if self.should_open_circuit(&stats) {
                    stats.state = CircuitState::Open;
                    warn_fmt!("CircuitBreakerFilter", "Circuit opened for target: {} (failures: {})",
                        target_key, stats.failure_count);
                }
            }
            CircuitState::HalfOpen => {
                // Failure in half-open state immediately opens the circuit
                stats.state = CircuitState::Open;
                stats.failure_count += 1;
                stats.success_count = 0;
                stats.last_failure_time = Some(Instant::now());
                warn_fmt!("CircuitBreakerFilter", "Circuit re-opened for target: {}", target_key);
            }
            CircuitState::Open => {
                // Already open, just update failure time
                stats.last_failure_time = Some(Instant::now());
            }
        }
    }
}

#[async_trait]
impl Filter for CircuitBreakerFilter {
    fn filter_type(&self) -> FilterType {
        FilterType::Both
    }

    fn name(&self) -> &str {
        "circuit_breaker"
    }

    async fn pre_filter(&self, request: ProxyRequest) -> Result<ProxyRequest, ProxyError> {
        let target_key = self.get_target_key(&request);
        let mut stats = self.stats.entry(target_key.clone()).or_default();

        match stats.state {
            CircuitState::Closed => {
                // Circuit is closed, allow request
                debug_fmt!("CircuitBreakerFilter", "Circuit closed, allowing request to: {}", target_key);
                Ok(request)
            }
            CircuitState::Open => {
                // Check if we should try half-open
                if self.should_try_half_open(&stats) {
                    stats.state = CircuitState::HalfOpen;
                    stats.success_count = 0;
                    info_fmt!("CircuitBreakerFilter", "Circuit transitioning to half-open for target: {}", target_key);
                    Ok(request)
                } else {
                    // Circuit is open, reject request
                    warn_fmt!("CircuitBreakerFilter", "Circuit open, rejecting request to: {}", target_key);
                    Err(ProxyError::FilterError(format!(
                        "Circuit breaker is open for target: {}. Failures: {}, Last failure: {:?}",
                        target_key, stats.failure_count, stats.last_failure_time
                    )))
                }
            }
            CircuitState::HalfOpen => {
                // Allow limited requests in half-open state
                debug_fmt!("CircuitBreakerFilter", "Circuit half-open, allowing test request to: {}", target_key);
                Ok(request)
            }
        }
    }

    async fn post_filter(&self, request: ProxyRequest, response: ProxyResponse) -> Result<ProxyResponse, ProxyError> {
        let target_key = self.get_target_key(&request);

        // Check if response indicates failure
        let is_failure = if self.config.fail_on_5xx && response.status >= 500 {
            true
        } else {
            false
        };

        if is_failure {
            self.record_failure(&target_key);
            debug_fmt!("CircuitBreakerFilter", "Recorded failure for target: {} (status: {})",
                target_key, response.status);
        } else {
            self.record_success(&target_key);
            debug_fmt!("CircuitBreakerFilter", "Recorded success for target: {} (status: {})",
                target_key, response.status);
        }

        Ok(response)
    }
}

/// Configuration for an enhanced rate limiting filter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnhancedRateLimitFilterConfig {
    /// Number of requests allowed per second
    pub req_per_sec: f64,
    /// Maximum burst size (number of requests that can be made immediately)
    #[serde(default = "default_burst_size")]
    pub burst_size: u32,
    /// Rate limiting strategy
    #[serde(default = "default_rate_limit_strategy")]
    pub strategy: RateLimitStrategy,
    /// Custom header to extract client key from
    pub client_key_header: Option<String>,
    /// Whether to include rate limit headers in response
    #[serde(default = "default_true")]
    pub include_headers: bool,
}

fn default_burst_size() -> u32 {
    10
}

fn default_rate_limit_strategy() -> RateLimitStrategy {
    RateLimitStrategy::Global
}

/// Rate limiting strategies
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitStrategy {
    /// Global rate limiting across all clients
    Global,
    /// Per-IP rate limiting
    PerIp,
    /// Per-header value rate limiting
    PerHeader,
}

impl Default for EnhancedRateLimitFilterConfig {
    fn default() -> Self {
        Self {
            req_per_sec: 10.0,
            burst_size: default_burst_size(),
            strategy: default_rate_limit_strategy(),
            client_key_header: None,
            include_headers: true,
        }
    }
}

/// An enhanced rate limiting filter with per-client support.
#[derive(Debug)]
pub struct EnhancedRateLimitFilter {
    config: EnhancedRateLimitFilterConfig,
    /// Token buckets per client key
    buckets: Arc<DashMap<String, Arc<Mutex<TokenBucket>>>>,
    /// Global token bucket for global strategy
    global_bucket: Arc<Mutex<TokenBucket>>,
}

impl EnhancedRateLimitFilter {
    /// Create a new enhanced rate limit filter with the given configuration.
    pub fn new(config: EnhancedRateLimitFilterConfig) -> Self {
        let global_bucket = TokenBucket::new(config.burst_size, config.req_per_sec);
        Self {
            config,
            buckets: Arc::new(DashMap::new()),
            global_bucket: Arc::new(Mutex::new(global_bucket)),
        }
    }

    /// Extract client key based on strategy
    async fn get_client_key(&self, request: &ProxyRequest) -> String {
        match &self.config.strategy {
            RateLimitStrategy::Global => "global".to_string(),
            RateLimitStrategy::PerIp => {
                let context = request.context.read().await;
                context.client_ip.clone().unwrap_or_else(|| "unknown".to_string())
            }
            RateLimitStrategy::PerHeader => {
                if let Some(header_name) = &self.config.client_key_header {
                    if let Some(header_value) = request.headers.get(header_name) {
                        if let Ok(value_str) = header_value.to_str() {
                            return value_str.to_string();
                        }
                    }
                }
                "unknown".to_string()
            }
        }
    }

    /// Get or create token bucket for client
    fn get_bucket(&self, client_key: &str) -> Arc<Mutex<TokenBucket>> {
        match &self.config.strategy {
            RateLimitStrategy::Global => self.global_bucket.clone(),
            _ => {
                self.buckets.entry(client_key.to_string())
                    .or_insert_with(|| {
                        Arc::new(Mutex::new(TokenBucket::new(
                            self.config.burst_size,
                            self.config.req_per_sec
                        )))
                    })
                    .clone()
            }
        }
    }

    /// Add rate limit headers to response
    fn add_rate_limit_headers(&self, headers: &mut HeaderMap, bucket: &TokenBucket) {
        if !self.config.include_headers {
            return;
        }

        // Add standard rate limit headers
        if let Ok(limit_header) = HeaderValue::from_str(&self.config.req_per_sec.to_string()) {
            headers.insert("X-RateLimit-Limit", limit_header);
        }

        if let Ok(remaining_header) = HeaderValue::from_str(&bucket.tokens.floor().to_string()) {
            headers.insert("X-RateLimit-Remaining", remaining_header);
        }

        // Calculate reset time (simplified)
        let reset_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() + 60; // Reset in 1 minute

        if let Ok(reset_header) = HeaderValue::from_str(&reset_time.to_string()) {
            headers.insert("X-RateLimit-Reset", reset_header);
        }
    }
}

#[async_trait]
impl Filter for EnhancedRateLimitFilter {
    fn filter_type(&self) -> FilterType {
        FilterType::Pre
    }

    fn name(&self) -> &str {
        "enhanced_rate_limit"
    }

    async fn pre_filter(&self, request: ProxyRequest) -> Result<ProxyRequest, ProxyError> {
        let client_key = self.get_client_key(&request).await;
        let bucket = self.get_bucket(&client_key);
        let mut bucket_guard = bucket.lock().await;

        if bucket_guard.try_consume() {
            // Request allowed
            debug_fmt!("EnhancedRateLimitFilter", "Request allowed for client: {} (path: {})",
                client_key, request.path);
            Ok(request)
        } else {
            // Rate limit exceeded - return 429 Too Many Requests
            warn_fmt!("EnhancedRateLimitFilter", "Rate limit exceeded for client: {} (path: {})",
                client_key, request.path);

            // Create a 429 response
            let mut headers = HeaderMap::new();
            self.add_rate_limit_headers(&mut headers, &bucket_guard);
            headers.insert("Retry-After", HeaderValue::from_static("60"));

            Err(ProxyError::RateLimitExceeded(format!(
                "Rate limit exceeded for client: {}. Limit: {} req/sec, Burst: {}",
                client_key, self.config.req_per_sec, self.config.burst_size
            )))
        }
    }
}

/// Cached response entry
#[derive(Debug, Clone)]
struct CacheEntry {
    status: u16,
    headers: HeaderMap,
    body: Bytes,
    expires_at: Instant,
}

impl CacheEntry {
    fn is_expired(&self) -> bool {
        Instant::now() > self.expires_at
    }
}

/// Configuration for a caching filter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachingFilterConfig {
    /// Time-to-live for cached responses in seconds
    #[serde(default = "default_ttl_secs")]
    pub ttl_secs: u64,
    /// Cache key pattern (supports {method}, {path}, {query} placeholders)
    #[serde(default = "default_cache_key")]
    pub cache_key: String,
    /// Maximum cache size (number of entries)
    #[serde(default = "default_max_cache_size")]
    pub max_cache_size: usize,
    /// Maximum response size to cache (in bytes)
    #[serde(default = "default_max_response_size")]
    pub max_response_size: usize,
    /// Whether to cache only GET requests
    #[serde(default = "default_true")]
    pub cache_get_only: bool,
    /// Status codes to cache
    #[serde(default = "default_cacheable_status_codes")]
    pub cacheable_status_codes: Vec<u16>,
}

fn default_ttl_secs() -> u64 {
    300 // 5 minutes
}

fn default_cache_key() -> String {
    "{method}:{path}:{query}".to_string()
}

fn default_max_cache_size() -> usize {
    1000
}

fn default_max_response_size() -> usize {
    1024 * 1024 // 1MB
}

fn default_cacheable_status_codes() -> Vec<u16> {
    vec![200, 203, 300, 301, 302, 404, 410]
}

impl Default for CachingFilterConfig {
    fn default() -> Self {
        Self {
            ttl_secs: default_ttl_secs(),
            cache_key: default_cache_key(),
            max_cache_size: default_max_cache_size(),
            max_response_size: default_max_response_size(),
            cache_get_only: true,
            cacheable_status_codes: default_cacheable_status_codes(),
        }
    }
}

/// A filter that caches GET responses with TTL-based expiration.
#[derive(Debug)]
pub struct CachingFilter {
    config: CachingFilterConfig,
    /// In-memory cache
    cache: Arc<DashMap<String, CacheEntry>>,
}

impl CachingFilter {
    /// Create a new caching filter with the given configuration.
    pub fn new(config: CachingFilterConfig) -> Self {
        Self {
            config,
            cache: Arc::new(DashMap::new()),
        }
    }

    /// Generate cache key from request
    fn generate_cache_key(&self, request: &ProxyRequest) -> String {
        let mut key = self.config.cache_key.clone();
        key = key.replace("{method}", &format!("{:?}", request.method));
        key = key.replace("{path}", &request.path);
        key = key.replace("{query}", &request.query.as_deref().unwrap_or(""));
        key
    }

    /// Check if request is cacheable
    fn is_cacheable_request(&self, request: &ProxyRequest) -> bool {
        if self.config.cache_get_only {
            matches!(request.method, crate::core::HttpMethod::Get)
        } else {
            true
        }
    }

    /// Check if response is cacheable
    fn is_cacheable_response(&self, response: &ProxyResponse) -> bool {
        self.config.cacheable_status_codes.contains(&response.status)
    }

    /// Clean expired entries from cache
    fn cleanup_expired(&self) {
        let _now = Instant::now();
        self.cache.retain(|_, entry| !entry.is_expired());

        // If cache is still too large, remove oldest entries
        if self.cache.len() > self.config.max_cache_size {
            let excess = self.cache.len() - self.config.max_cache_size;
            let mut keys_to_remove = Vec::new();

            for entry in self.cache.iter().take(excess) {
                keys_to_remove.push(entry.key().clone());
            }

            for key in keys_to_remove {
                self.cache.remove(&key);
            }
        }
    }

    /// Create a cached response
    fn create_cached_response(&self, entry: &CacheEntry) -> ProxyResponse {
        ProxyResponse {
            status: entry.status,
            headers: entry.headers.clone(),
            body: reqwest::Body::from(entry.body.clone()),
            context: Arc::new(tokio::sync::RwLock::new(crate::core::ResponseContext::default())),
        }
    }
}

#[async_trait]
impl Filter for CachingFilter {
    fn filter_type(&self) -> FilterType {
        FilterType::Both
    }

    fn name(&self) -> &str {
        "caching"
    }

    async fn pre_filter(&self, request: ProxyRequest) -> Result<ProxyRequest, ProxyError> {
        if !self.is_cacheable_request(&request) {
            debug_fmt!("CachingFilter", "Request not cacheable: {} {}", request.method, request.path);
            return Ok(request);
        }

        let cache_key = self.generate_cache_key(&request);

        // Check if we have a cached response
        if let Some(entry) = self.cache.get(&cache_key) {
            if !entry.is_expired() {
                debug_fmt!("CachingFilter", "Cache hit for key: {}", cache_key);

                // Create response from cache
                let _cached_response = self.create_cached_response(&entry);

                // Store cached response in request context for immediate return
                {
                    let mut context = request.context.write().await;
                    context.attributes.insert("cached_response".to_string(),
                        serde_json::Value::Bool(true));
                }

                // Note: In a real implementation, we'd need a way to short-circuit
                // the request pipeline here. For now, we'll handle this in post_filter
                return Ok(request);
            } else {
                // Remove expired entry
                self.cache.remove(&cache_key);
                debug_fmt!("CachingFilter", "Removed expired cache entry for key: {}", cache_key);
            }
        }

        debug_fmt!("CachingFilter", "Cache miss for key: {}", cache_key);

        // Store cache key in request context for post_filter
        {
            let mut context = request.context.write().await;
            context.attributes.insert("cache_key".to_string(),
                serde_json::Value::String(cache_key));
        }

        Ok(request)
    }

    async fn post_filter(&self, request: ProxyRequest, response: ProxyResponse) -> Result<ProxyResponse, ProxyError> {
        // Check if this was a cacheable request
        let context = request.context.read().await;
        let cache_key = if let Some(serde_json::Value::String(key)) = context.attributes.get("cache_key") {
            key.clone()
        } else {
            return Ok(response);
        };
        drop(context);

        if !self.is_cacheable_response(&response) {
            debug_fmt!("CachingFilter", "Response not cacheable for key: {} (status: {})",
                cache_key, response.status);
            return Ok(response);
        }

        // Read response body for caching
        let (new_body, data) = tee_body(response.body, self.config.max_response_size).await?;

        if data.len() > self.config.max_response_size {
            debug_fmt!("CachingFilter", "Response too large to cache: {} bytes", data.len());
            return Ok(ProxyResponse {
                status: response.status,
                headers: response.headers,
                body: new_body,
                context: response.context,
            });
        }

        // Create cache entry
        let expires_at = Instant::now() + Duration::from_secs(self.config.ttl_secs);
        let entry = CacheEntry {
            status: response.status,
            headers: response.headers.clone(),
            body: Bytes::from(data.into_bytes()),
            expires_at,
        };

        // Store in cache
        self.cache.insert(cache_key.clone(), entry);
        debug_fmt!("CachingFilter", "Cached response for key: {} (TTL: {}s)",
            cache_key, self.config.ttl_secs);

        // Cleanup expired entries periodically
        if self.cache.len() % 100 == 0 {
            self.cleanup_expired();
        }

        Ok(ProxyResponse {
            status: response.status,
            headers: response.headers,
            body: new_body,
            context: response.context,
        })
    }
}

/// Configuration for an authentication filter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthFilterConfig {
    /// JWT issuer to validate
    pub jwt_issuer: String,
    /// JWT audience to validate
    pub jwt_audience: String,
    /// JWKS URL for fetching public keys
    pub jwks_url: Option<String>,
    /// Static JWT secret for HMAC validation
    pub jwt_secret: Option<String>,
    /// JWT algorithm to use
    #[serde(default = "default_jwt_algorithm")]
    pub jwt_algorithm: String,
    /// Paths that bypass authentication
    #[serde(default = "default_bypass_paths")]
    pub bypass_paths: Vec<String>,
    /// Header name to extract JWT from
    #[serde(default = "default_auth_header")]
    pub auth_header: String,
    /// Whether to validate JWT expiration
    #[serde(default = "default_true")]
    pub validate_exp: bool,
    /// Whether to validate JWT not-before
    #[serde(default = "default_true")]
    pub validate_nbf: bool,
    /// Leeway for time-based validations (in seconds)
    #[serde(default = "default_leeway")]
    pub leeway: u64,
}

fn default_jwt_algorithm() -> String {
    "RS256".to_string()
}

fn default_bypass_paths() -> Vec<String> {
    vec!["/health".to_string(), "/metrics".to_string()]
}

fn default_auth_header() -> String {
    "authorization".to_string()
}

fn default_leeway() -> u64 {
    60 // 1 minute
}

impl Default for AuthFilterConfig {
    fn default() -> Self {
        Self {
            jwt_issuer: "https://example.com".to_string(),
            jwt_audience: "api".to_string(),
            jwks_url: None,
            jwt_secret: None,
            jwt_algorithm: default_jwt_algorithm(),
            bypass_paths: default_bypass_paths(),
            auth_header: default_auth_header(),
            validate_exp: true,
            validate_nbf: true,
            leeway: default_leeway(),
        }
    }
}

/// A filter that validates JWT tokens for authentication.
pub struct AuthFilter {
    config: AuthFilterConfig,
    /// Cached JWKS keys (using String representation for Debug)
    jwks_cache: Arc<DashMap<String, String>>,
    /// HTTP client for JWKS fetching
    client: reqwest::Client,
}

impl std::fmt::Debug for AuthFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthFilter")
            .field("config", &self.config)
            .field("jwks_cache_size", &self.jwks_cache.len())
            .field("client", &"reqwest::Client")
            .finish()
    }
}

impl AuthFilter {
    /// Create a new auth filter with the given configuration.
    pub fn new(config: AuthFilterConfig) -> Self {
        Self {
            config,
            jwks_cache: Arc::new(DashMap::new()),
            client: reqwest::Client::new(),
        }
    }

    /// Check if path should bypass authentication
    fn should_bypass(&self, path: &str) -> bool {
        for bypass_path in &self.config.bypass_paths {
            if path.starts_with(bypass_path) {
                return true;
            }
        }
        false
    }

    /// Extract JWT token from request
    fn extract_token(&self, request: &ProxyRequest) -> Option<String> {
        if let Some(auth_header) = request.headers.get(&self.config.auth_header) {
            if let Ok(auth_str) = auth_header.to_str() {
                if auth_str.starts_with("Bearer ") {
                    return Some(auth_str[7..].to_string());
                }
                return Some(auth_str.to_string());
            }
        }
        None
    }

    /// Get decoding key for JWT validation
    async fn get_decoding_key(&self, token: &str) -> Result<DecodingKey, ProxyError> {
        // If we have a static secret, use it
        if let Some(secret) = &self.config.jwt_secret {
            return Ok(DecodingKey::from_secret(secret.as_bytes()));
        }

        // Otherwise, try to get key from JWKS
        if let Some(jwks_url) = &self.config.jwks_url {
            // Decode header to get key ID
            let header = decode_header(token)
                .map_err(|e| ProxyError::SecurityError(format!("Failed to decode JWT header: {}", e)))?;

            let kid = header.kid.ok_or_else(||
                ProxyError::SecurityError("JWT header missing 'kid' field".to_string()))?;

            // Check cache first (simplified - in production, cache the actual key)
            if self.jwks_cache.contains_key(&kid) {
                // For now, just recreate the key from the cached data
                // In production, you'd cache the actual DecodingKey
                if let Some(secret) = &self.config.jwt_secret {
                    return Ok(DecodingKey::from_secret(secret.as_bytes()));
                }
            }

            // Fetch JWKS and extract key
            let jwks_response = self.client.get(jwks_url)
                .send()
                .await
                .map_err(|e| ProxyError::SecurityError(format!("Failed to fetch JWKS: {}", e)))?;

            let jwks: Value = jwks_response.json()
                .await
                .map_err(|e| ProxyError::SecurityError(format!("Failed to parse JWKS: {}", e)))?;

            // Find the key with matching kid
            if let Some(keys) = jwks.get("keys").and_then(|k| k.as_array()) {
                for key in keys {
                    if let Some(key_kid) = key.get("kid").and_then(|k| k.as_str()) {
                        if key_kid == kid {
                            // Extract the key material (simplified - in production, handle different key types)
                            if let Some(n) = key.get("n").and_then(|n| n.as_str()) {
                                if let Some(e) = key.get("e").and_then(|e| e.as_str()) {
                                    // For RSA keys, we'd need to construct the key from n and e
                                    // This is a simplified implementation
                                    let decoding_key = DecodingKey::from_rsa_components(n, e)
                                        .map_err(|e| ProxyError::SecurityError(format!("Failed to create RSA key: {}", e)))?;

                                    // Cache the key (simplified - store kid for now)
                                    self.jwks_cache.insert(kid.clone(), format!("{}:{}", n, e));
                                    return Ok(decoding_key);
                                }
                            }
                        }
                    }
                }
            }

            return Err(ProxyError::SecurityError(format!("Key with kid '{}' not found in JWKS", kid)));
        }

        Err(ProxyError::SecurityError("No JWT secret or JWKS URL configured".to_string()))
    }

    /// Validate JWT token
    async fn validate_token(&self, token: &str) -> Result<Value, ProxyError> {
        let decoding_key = self.get_decoding_key(token).await?;

        let algorithm = match self.config.jwt_algorithm.as_str() {
            "HS256" => Algorithm::HS256,
            "HS384" => Algorithm::HS384,
            "HS512" => Algorithm::HS512,
            "RS256" => Algorithm::RS256,
            "RS384" => Algorithm::RS384,
            "RS512" => Algorithm::RS512,
            "ES256" => Algorithm::ES256,
            "ES384" => Algorithm::ES384,
            _ => return Err(ProxyError::SecurityError(format!("Unsupported JWT algorithm: {}", self.config.jwt_algorithm))),
        };

        let mut validation = Validation::new(algorithm);
        validation.set_issuer(&[&self.config.jwt_issuer]);
        validation.set_audience(&[&self.config.jwt_audience]);
        validation.validate_exp = self.config.validate_exp;
        validation.validate_nbf = self.config.validate_nbf;
        validation.leeway = self.config.leeway;

        let token_data = decode::<Value>(token, &decoding_key, &validation)
            .map_err(|e| ProxyError::SecurityError(format!("JWT validation failed: {}", e)))?;

        Ok(token_data.claims)
    }
}

#[async_trait]
impl Filter for AuthFilter {
    fn filter_type(&self) -> FilterType {
        FilterType::Pre
    }

    fn name(&self) -> &str {
        "auth"
    }

    async fn pre_filter(&self, request: ProxyRequest) -> Result<ProxyRequest, ProxyError> {
        // Check if path should bypass authentication
        if self.should_bypass(&request.path) {
            debug_fmt!("AuthFilter", "Bypassing authentication for path: {}", request.path);
            return Ok(request);
        }

        // Extract JWT token
        let token = self.extract_token(&request)
            .ok_or_else(|| ProxyError::SecurityError("Missing or invalid authorization header".to_string()))?;

        // Validate token
        let claims = self.validate_token(&token).await?;

        // Store claims in request context
        {
            let mut context = request.context.write().await;
            context.attributes.insert("jwt_claims".to_string(), claims);
            context.attributes.insert("authenticated".to_string(), serde_json::Value::Bool(true));
        }

        debug_fmt!("AuthFilter", "Successfully authenticated request for path: {}", request.path);
        Ok(request)
    }
}

/// Configuration for a CORS filter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorsFilterConfig {
    /// Allowed origins (use "*" for all origins)
    #[serde(default = "default_allowed_origins")]
    pub allowed_origins: Vec<String>,
    /// Allowed HTTP methods
    #[serde(default = "default_allowed_methods")]
    pub allowed_methods: Vec<String>,
    /// Allowed headers
    #[serde(default = "default_allowed_headers")]
    pub allowed_headers: Vec<String>,
    /// Exposed headers
    #[serde(default = "default_exposed_headers")]
    pub exposed_headers: Vec<String>,
    /// Whether to allow credentials
    #[serde(default = "default_false")]
    pub allow_credentials: bool,
    /// Max age for preflight cache (in seconds)
    #[serde(default = "default_max_age")]
    pub max_age: u32,
}

fn default_allowed_origins() -> Vec<String> {
    vec!["*".to_string()]
}

fn default_allowed_methods() -> Vec<String> {
    vec!["GET".to_string(), "POST".to_string(), "PUT".to_string(), "DELETE".to_string(), "OPTIONS".to_string()]
}

fn default_allowed_headers() -> Vec<String> {
    vec!["Content-Type".to_string(), "Authorization".to_string(), "X-Requested-With".to_string()]
}

fn default_exposed_headers() -> Vec<String> {
    vec![]
}

fn default_max_age() -> u32 {
    86400 // 24 hours
}

impl Default for CorsFilterConfig {
    fn default() -> Self {
        Self {
            allowed_origins: default_allowed_origins(),
            allowed_methods: default_allowed_methods(),
            allowed_headers: default_allowed_headers(),
            exposed_headers: default_exposed_headers(),
            allow_credentials: false,
            max_age: default_max_age(),
        }
    }
}

/// A filter that handles CORS (Cross-Origin Resource Sharing) headers.
#[derive(Debug)]
pub struct CorsFilter {
    config: CorsFilterConfig,
}

impl CorsFilter {
    /// Create a new CORS filter with the given configuration.
    pub fn new(config: CorsFilterConfig) -> Self {
        Self { config }
    }

    /// Check if origin is allowed
    fn is_origin_allowed(&self, origin: &str) -> bool {
        self.config.allowed_origins.contains(&"*".to_string()) ||
        self.config.allowed_origins.contains(&origin.to_string())
    }

    /// Get the allowed origin for the request
    fn get_allowed_origin(&self, request_origin: Option<&str>) -> Option<String> {
        if let Some(origin) = request_origin {
            if self.is_origin_allowed(origin) {
                if self.config.allowed_origins.contains(&"*".to_string()) && !self.config.allow_credentials {
                    return Some("*".to_string());
                } else {
                    return Some(origin.to_string());
                }
            }
        }
        None
    }

    /// Add CORS headers to response
    fn add_cors_headers(&self, headers: &mut HeaderMap, request_origin: Option<&str>) {
        // Access-Control-Allow-Origin
        if let Some(origin) = self.get_allowed_origin(request_origin) {
            if let Ok(origin_value) = HeaderValue::from_str(&origin) {
                headers.insert("Access-Control-Allow-Origin", origin_value);
            }
        }

        // Access-Control-Allow-Methods
        if !self.config.allowed_methods.is_empty() {
            let methods = self.config.allowed_methods.join(", ");
            if let Ok(methods_value) = HeaderValue::from_str(&methods) {
                headers.insert("Access-Control-Allow-Methods", methods_value);
            }
        }

        // Access-Control-Allow-Headers
        if !self.config.allowed_headers.is_empty() {
            let headers_str = self.config.allowed_headers.join(", ");
            if let Ok(headers_value) = HeaderValue::from_str(&headers_str) {
                headers.insert("Access-Control-Allow-Headers", headers_value);
            }
        }

        // Access-Control-Expose-Headers
        if !self.config.exposed_headers.is_empty() {
            let exposed = self.config.exposed_headers.join(", ");
            if let Ok(exposed_value) = HeaderValue::from_str(&exposed) {
                headers.insert("Access-Control-Expose-Headers", exposed_value);
            }
        }

        // Access-Control-Allow-Credentials
        if self.config.allow_credentials {
            headers.insert("Access-Control-Allow-Credentials", HeaderValue::from_static("true"));
        }

        // Access-Control-Max-Age
        if let Ok(max_age_value) = HeaderValue::from_str(&self.config.max_age.to_string()) {
            headers.insert("Access-Control-Max-Age", max_age_value);
        }
    }

    /// Handle preflight request
    fn handle_preflight(&self, request: &ProxyRequest) -> Result<ProxyResponse, ProxyError> {
        let request_origin = request.headers.get("origin")
            .and_then(|h| h.to_str().ok());

        if !self.get_allowed_origin(request_origin).is_some() {
            return Err(ProxyError::SecurityError("Origin not allowed".to_string()));
        }

        let mut headers = HeaderMap::new();
        self.add_cors_headers(&mut headers, request_origin);

        Ok(ProxyResponse {
            status: 204, // No Content
            headers,
            body: reqwest::Body::from(Vec::new()),
            context: Arc::new(tokio::sync::RwLock::new(crate::core::ResponseContext::default())),
        })
    }
}

#[async_trait]
impl Filter for CorsFilter {
    fn filter_type(&self) -> FilterType {
        FilterType::Both
    }

    fn name(&self) -> &str {
        "cors"
    }

    async fn pre_filter(&self, request: ProxyRequest) -> Result<ProxyRequest, ProxyError> {
        // Handle preflight requests (OPTIONS method)
        if matches!(request.method, crate::core::HttpMethod::Options) {
            debug_fmt!("CorsFilter", "Handling CORS preflight request for path: {}", request.path);

            // Store preflight response in context
            let _preflight_response = self.handle_preflight(&request)?;
            {
                let mut context = request.context.write().await;
                context.attributes.insert("cors_preflight".to_string(), serde_json::Value::Bool(true));
                // Note: In a real implementation, we'd need a way to return the preflight response immediately
            }
        }

        Ok(request)
    }

    async fn post_filter(&self, request: ProxyRequest, mut response: ProxyResponse) -> Result<ProxyResponse, ProxyError> {
        let request_origin = request.headers.get("origin")
            .and_then(|h| h.to_str().ok());

        // Add CORS headers to response
        self.add_cors_headers(&mut response.headers, request_origin);

        debug_fmt!("CorsFilter", "Added CORS headers to response for origin: {:?}", request_origin);
        Ok(response)
    }
}

/// Configuration for a metrics filter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsFilterConfig {
    /// Metrics endpoint path
    #[serde(default = "default_metrics_endpoint")]
    pub metrics_endpoint: String,
    /// Histogram buckets for latency measurements
    #[serde(default = "default_histogram_buckets")]
    pub histogram_buckets: Vec<f64>,
    /// Whether to include path labels in metrics
    #[serde(default = "default_true")]
    pub include_path_labels: bool,
    /// Whether to include method labels in metrics
    #[serde(default = "default_true")]
    pub include_method_labels: bool,
    /// Whether to include status labels in metrics
    #[serde(default = "default_true")]
    pub include_status_labels: bool,
}

fn default_metrics_endpoint() -> String {
    "/metrics".to_string()
}

fn default_histogram_buckets() -> Vec<f64> {
    vec![0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]
}

impl Default for MetricsFilterConfig {
    fn default() -> Self {
        Self {
            metrics_endpoint: default_metrics_endpoint(),
            histogram_buckets: default_histogram_buckets(),
            include_path_labels: true,
            include_method_labels: true,
            include_status_labels: true,
        }
    }
}

/// A filter that records metrics for requests and responses.
#[derive(Debug)]
pub struct MetricsFilter {
    config: MetricsFilterConfig,
    /// Prometheus registry
    registry: Arc<Registry>,
    /// Request counter
    request_counter: Counter,
    /// Request duration histogram
    request_duration: Histogram,
    /// Response size histogram
    response_size: Histogram,
}

impl MetricsFilter {
    /// Create a new metrics filter with the given configuration.
    pub fn new(config: MetricsFilterConfig) -> Result<Self, ProxyError> {
        let registry = Arc::new(Registry::new());

        // Create request counter
        let request_counter = Counter::with_opts(
            Opts::new("http_requests_total", "Total number of HTTP requests")
                .namespace("foxy")
        ).map_err(|e| ProxyError::FilterError(format!("Failed to create request counter: {}", e)))?;

        // Create request duration histogram
        let request_duration = Histogram::with_opts(
            HistogramOpts::new("http_request_duration_seconds", "HTTP request duration in seconds")
                .namespace("foxy")
                .buckets(config.histogram_buckets.clone())
        ).map_err(|e| ProxyError::FilterError(format!("Failed to create duration histogram: {}", e)))?;

        // Create response size histogram
        let response_size = Histogram::with_opts(
            HistogramOpts::new("http_response_size_bytes", "HTTP response size in bytes")
                .namespace("foxy")
                .buckets(vec![100.0, 1000.0, 10000.0, 100000.0, 1000000.0, 10000000.0])
        ).map_err(|e| ProxyError::FilterError(format!("Failed to create size histogram: {}", e)))?;

        // Register metrics
        registry.register(Box::new(request_counter.clone()))
            .map_err(|e| ProxyError::FilterError(format!("Failed to register request counter: {}", e)))?;
        registry.register(Box::new(request_duration.clone()))
            .map_err(|e| ProxyError::FilterError(format!("Failed to register duration histogram: {}", e)))?;
        registry.register(Box::new(response_size.clone()))
            .map_err(|e| ProxyError::FilterError(format!("Failed to register size histogram: {}", e)))?;

        Ok(Self {
            config,
            registry,
            request_counter,
            request_duration,
            response_size,
        })
    }

    /// Get metrics endpoint response
    fn get_metrics_response(&self) -> Result<ProxyResponse, ProxyError> {
        use prometheus::Encoder;

        let encoder = prometheus::TextEncoder::new();
        let metric_families = self.registry.gather();
        let mut buffer = Vec::new();

        encoder.encode(&metric_families, &mut buffer)
            .map_err(|e| ProxyError::FilterError(format!("Failed to encode metrics: {}", e)))?;

        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"));

        Ok(ProxyResponse {
            status: 200,
            headers,
            body: reqwest::Body::from(buffer),
            context: Arc::new(tokio::sync::RwLock::new(crate::core::ResponseContext::default())),
        })
    }

    /// Extract labels from request
    fn get_labels(&self, request: &ProxyRequest, status: Option<u16>) -> Vec<(&str, String)> {
        let mut labels = Vec::new();

        if self.config.include_method_labels {
            labels.push(("method", format!("{:?}", request.method)));
        }

        if self.config.include_path_labels {
            // Normalize path to avoid high cardinality
            let normalized_path = self.normalize_path(&request.path);
            labels.push(("path", normalized_path));
        }

        if self.config.include_status_labels {
            if let Some(status_code) = status {
                labels.push(("status", status_code.to_string()));
            }
        }

        labels
    }

    /// Normalize path to reduce cardinality
    fn normalize_path(&self, path: &str) -> String {
        // Simple normalization - replace numeric segments with placeholders
        let segments: Vec<&str> = path.split('/').collect();
        let normalized_segments: Vec<String> = segments.iter().map(|segment| {
            if segment.chars().all(|c| c.is_ascii_digit()) {
                "{id}".to_string()
            } else {
                segment.to_string()
            }
        }).collect();
        normalized_segments.join("/")
    }
}

#[async_trait]
impl Filter for MetricsFilter {
    fn filter_type(&self) -> FilterType {
        FilterType::Both
    }

    fn name(&self) -> &str {
        "metrics"
    }

    async fn pre_filter(&self, request: ProxyRequest) -> Result<ProxyRequest, ProxyError> {
        // Check if this is a metrics endpoint request
        if request.path == self.config.metrics_endpoint {
            debug_fmt!("MetricsFilter", "Serving metrics endpoint");

            // Store metrics response in context
            {
                let mut context = request.context.write().await;
                context.attributes.insert("serve_metrics".to_string(), serde_json::Value::Bool(true));
            }
        }

        // Record request start time
        {
            let mut context = request.context.write().await;
            context.start_time = Some(Instant::now());
        }

        // Increment request counter
        self.request_counter.inc();

        debug_fmt!("MetricsFilter", "Recorded request start for: {} {}", request.method, request.path);
        Ok(request)
    }

    async fn post_filter(&self, request: ProxyRequest, response: ProxyResponse) -> Result<ProxyResponse, ProxyError> {
        // Check if we should serve metrics
        {
            let context = request.context.read().await;
            if let Some(serde_json::Value::Bool(true)) = context.attributes.get("serve_metrics") {
                return self.get_metrics_response();
            }
        }

        // Calculate request duration
        let duration = {
            let context = request.context.read().await;
            if let Some(start_time) = context.start_time {
                start_time.elapsed().as_secs_f64()
            } else {
                0.0
            }
        };

        // Record metrics
        self.request_duration.observe(duration);

        // Record response size if available
        if let Some(content_length) = response.headers.get("content-length") {
            if let Ok(size_str) = content_length.to_str() {
                if let Ok(size) = size_str.parse::<f64>() {
                    self.response_size.observe(size);
                }
            }
        }

        debug_fmt!("MetricsFilter", "Recorded metrics for: {} {} (duration: {:.3}s, status: {})",
            request.method, request.path, duration, response.status);

        Ok(response)
    }
}

/// Transformation types for body transformation
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TransformationType {
    /// Regex-based replacement
    RegexReplace {
        pattern: String,
        replacement: String,
        #[serde(default = "default_false")]
        global: bool,
    },
    /// JSON path-based transformation
    JsonPath {
        path: String,
        value: serde_json::Value,
        operation: JsonOperation,
    },
    /// Base64 encoding/decoding
    Base64 {
        #[serde(default = "default_false")]
        decode: bool,
    },
}

/// JSON operations for JsonPath transformation
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JsonOperation {
    Set,
    Delete,
    Append,
}

/// Configuration for a body transformation filter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BodyTransformFilterConfig {
    /// Transformation to apply
    pub transformation: TransformationType,
    /// Whether to transform request body
    #[serde(default = "default_false")]
    pub transform_request: bool,
    /// Whether to transform response body
    #[serde(default = "default_true")]
    pub transform_response: bool,
    /// Content types to transform (empty means all)
    #[serde(default = "default_transform_content_types")]
    pub content_types: Vec<String>,
    /// Maximum body size to transform (in bytes)
    #[serde(default = "default_max_transform_size")]
    pub max_transform_size: usize,
}

fn default_transform_content_types() -> Vec<String> {
    vec![
        "text/plain".to_string(),
        "text/html".to_string(),
        "application/json".to_string(),
        "application/xml".to_string(),
    ]
}

fn default_max_transform_size() -> usize {
    1024 * 1024 // 1MB
}

impl Default for BodyTransformFilterConfig {
    fn default() -> Self {
        Self {
            transformation: TransformationType::RegexReplace {
                pattern: "old".to_string(),
                replacement: "new".to_string(),
                global: false,
            },
            transform_request: false,
            transform_response: true,
            content_types: default_transform_content_types(),
            max_transform_size: default_max_transform_size(),
        }
    }
}

/// A filter that transforms request/response bodies using various methods.
#[derive(Debug)]
pub struct BodyTransformFilter {
    config: BodyTransformFilterConfig,
    /// Compiled regex for regex transformations
    regex: Option<Regex>,
}

impl BodyTransformFilter {
    /// Create a new body transform filter with the given configuration.
    pub fn new(config: BodyTransformFilterConfig) -> Result<Self, ProxyError> {
        let regex = match &config.transformation {
            TransformationType::RegexReplace { pattern, .. } => {
                Some(Regex::new(pattern)
                    .map_err(|e| ProxyError::FilterError(format!("Invalid regex pattern: {}", e)))?)
            }
            _ => None,
        };

        Ok(Self { config, regex })
    }

    /// Check if content type should be transformed
    fn should_transform_content_type(&self, content_type: Option<&HeaderValue>) -> bool {
        if self.config.content_types.is_empty() {
            return true; // Transform all if no specific types configured
        }

        if let Some(ct) = content_type {
            if let Ok(ct_str) = ct.to_str() {
                return self.config.content_types.iter().any(|allowed| ct_str.contains(allowed));
            }
        }
        false
    }

    /// Apply transformation to body content
    fn transform_body(&self, body: &str) -> Result<String, ProxyError> {
        match &self.config.transformation {
            TransformationType::RegexReplace { replacement, global, .. } => {
                if let Some(regex) = &self.regex {
                    if *global {
                        Ok(regex.replace_all(body, replacement).to_string())
                    } else {
                        Ok(regex.replace(body, replacement).to_string())
                    }
                } else {
                    Err(ProxyError::FilterError("Regex not compiled".to_string()))
                }
            }
            TransformationType::JsonPath { path, value, operation } => {
                self.transform_json(body, path, value, operation)
            }
            TransformationType::Base64 { decode } => {
                if *decode {
                    use base64::{Engine as _, engine::general_purpose};
                    general_purpose::STANDARD.decode(body)
                        .map_err(|e| ProxyError::FilterError(format!("Base64 decode failed: {}", e)))
                        .and_then(|bytes| String::from_utf8(bytes)
                            .map_err(|e| ProxyError::FilterError(format!("UTF-8 decode failed: {}", e))))
                } else {
                    use base64::{Engine as _, engine::general_purpose};
                    Ok(general_purpose::STANDARD.encode(body))
                }
            }
        }
    }

    /// Transform JSON using JSONPath
    fn transform_json(&self, body: &str, path: &str, value: &serde_json::Value, operation: &JsonOperation) -> Result<String, ProxyError> {
        let mut json: serde_json::Value = serde_json::from_str(body)
            .map_err(|e| ProxyError::FilterError(format!("Failed to parse JSON: {}", e)))?;

        // Simple JSONPath implementation (for demonstration)
        // In production, use a proper JSONPath library
        let path_parts: Vec<&str> = path.split('.').collect();

        match operation {
            JsonOperation::Set => {
                self.set_json_value(&mut json, &path_parts, value.clone())?;
            }
            JsonOperation::Delete => {
                self.delete_json_value(&mut json, &path_parts)?;
            }
            JsonOperation::Append => {
                self.append_json_value(&mut json, &path_parts, value.clone())?;
            }
        }

        serde_json::to_string(&json)
            .map_err(|e| ProxyError::FilterError(format!("Failed to serialize JSON: {}", e)))
    }

    /// Set JSON value at path
    fn set_json_value(&self, json: &mut serde_json::Value, path: &[&str], value: serde_json::Value) -> Result<(), ProxyError> {
        if path.is_empty() {
            return Err(ProxyError::FilterError("Empty JSON path".to_string()));
        }

        let mut current = json;
        for &key in &path[..path.len() - 1] {
            current = current.get_mut(key)
                .ok_or_else(|| ProxyError::FilterError(format!("JSON path not found: {}", key)))?;
        }

        if let Some(obj) = current.as_object_mut() {
            obj.insert(path[path.len() - 1].to_string(), value);
        } else {
            return Err(ProxyError::FilterError("Cannot set value on non-object".to_string()));
        }

        Ok(())
    }

    /// Delete JSON value at path
    fn delete_json_value(&self, json: &mut serde_json::Value, path: &[&str]) -> Result<(), ProxyError> {
        if path.is_empty() {
            return Err(ProxyError::FilterError("Empty JSON path".to_string()));
        }

        let mut current = json;
        for &key in &path[..path.len() - 1] {
            current = current.get_mut(key)
                .ok_or_else(|| ProxyError::FilterError(format!("JSON path not found: {}", key)))?;
        }

        if let Some(obj) = current.as_object_mut() {
            obj.remove(path[path.len() - 1]);
        } else {
            return Err(ProxyError::FilterError("Cannot delete from non-object".to_string()));
        }

        Ok(())
    }

    /// Append JSON value at path (for arrays)
    fn append_json_value(&self, json: &mut serde_json::Value, path: &[&str], value: serde_json::Value) -> Result<(), ProxyError> {
        if path.is_empty() {
            return Err(ProxyError::FilterError("Empty JSON path".to_string()));
        }

        let mut current = json;
        for &key in path {
            current = current.get_mut(key)
                .ok_or_else(|| ProxyError::FilterError(format!("JSON path not found: {}", key)))?;
        }

        if let Some(arr) = current.as_array_mut() {
            arr.push(value);
        } else {
            return Err(ProxyError::FilterError("Cannot append to non-array".to_string()));
        }

        Ok(())
    }
}

#[async_trait]
impl Filter for BodyTransformFilter {
    fn filter_type(&self) -> FilterType {
        FilterType::Both
    }

    fn name(&self) -> &str {
        "body_transform"
    }

    async fn pre_filter(&self, mut request: ProxyRequest) -> Result<ProxyRequest, ProxyError> {
        if !self.config.transform_request {
            return Ok(request);
        }

        if !self.should_transform_content_type(request.headers.get(CONTENT_TYPE)) {
            debug_fmt!("BodyTransformFilter", "Skipping request transformation - content type not allowed");
            return Ok(request);
        }

        let (new_body, data) = tee_body(request.body, self.config.max_transform_size).await?;

        if data.len() > self.config.max_transform_size {
            debug_fmt!("BodyTransformFilter", "Request body too large to transform: {} bytes", data.len());
            request.body = new_body;
            return Ok(request);
        }

        if !data.is_empty() {
            let transformed = self.transform_body(&data)?;
            let transformed_len = transformed.len();
            request.body = reqwest::Body::from(transformed.into_bytes());
            debug_fmt!("BodyTransformFilter", "Transformed request body: {} -> {} bytes",
                data.len(), transformed_len);
        } else {
            request.body = new_body;
        }

        Ok(request)
    }

    async fn post_filter(&self, _request: ProxyRequest, mut response: ProxyResponse) -> Result<ProxyResponse, ProxyError> {
        if !self.config.transform_response {
            return Ok(response);
        }

        if !self.should_transform_content_type(response.headers.get(CONTENT_TYPE)) {
            debug_fmt!("BodyTransformFilter", "Skipping response transformation - content type not allowed");
            return Ok(response);
        }

        let (new_body, data) = tee_body(response.body, self.config.max_transform_size).await?;

        if data.len() > self.config.max_transform_size {
            debug_fmt!("BodyTransformFilter", "Response body too large to transform: {} bytes", data.len());
            response.body = new_body;
            return Ok(response);
        }

        if !data.is_empty() {
            let transformed = self.transform_body(&data)?;
            let transformed_len = transformed.len();

            // Update content length
            response.headers.insert("content-length",
                HeaderValue::from_str(&transformed_len.to_string())
                    .map_err(|e| ProxyError::FilterError(format!("Failed to set content-length: {}", e)))?);

            response.body = reqwest::Body::from(transformed.into_bytes());
            debug_fmt!("BodyTransformFilter", "Transformed response body: {} -> {} bytes",
                data.len(), transformed_len);
        } else {
            response.body = new_body;
        }

        Ok(response)
    }
}



/// Factory for creating filters based on configuration.
#[derive(Debug)]
pub struct FilterFactory;

impl FilterFactory {
    /// Create a filter based on the filter type and configuration.
    pub fn create_filter(filter_type: &str, config: serde_json::Value) -> Result<Arc<dyn Filter>, ProxyError> {
        debug_fmt!("Filter", "Creating filter of type '{}' with config: {}", filter_type, config);

        // See if we've got an external filter registered of that name
        if let Some(ctor) = get_registered_filter(filter_type) {
            return ctor(config);
        }
        
        match filter_type {
            "logging" => {
                let config: LoggingFilterConfig = serde_json::from_value(config)
                    .map_err(|e| {
                        let err = ProxyError::FilterError(format!("Invalid logging filter config: {e}"));
                        error_fmt!("Filter", "{}", err);
                        err
                    })?;
                Ok(Arc::new(LoggingFilter::new(config)))
            },
            "header" => {
                let config: HeaderFilterConfig = serde_json::from_value(config)
                    .map_err(|e| {
                        let err = ProxyError::FilterError(format!("Invalid header filter config: {e}"));
                        error_fmt!("Filter", "{}", err);
                        err
                    })?;
                Ok(Arc::new(HeaderFilter::new(config)))
            },
            "timeout" => {
                let config: TimeoutFilterConfig = serde_json::from_value(config)
                    .map_err(|e| {
                        let err = ProxyError::FilterError(format!("Invalid timeout filter config: {e}"));
                        error_fmt!("Filter", "{}", err);
                        err
                    })?;
                Ok(Arc::new(TimeoutFilter::new(config)))
            },
            "path_rewrite" => {
                let config: PathRewriteFilterConfig = serde_json::from_value(config)
                    .map_err(|e| {
                        let err = ProxyError::FilterError(format!("Invalid path rewrite filter config: {e}"));
                        error_fmt!("Filter", "{}", err);
                        err
                    })?;

                match PathRewriteFilter::new(config) {
                    Ok(filter) => Ok(Arc::new(filter)),
                    Err(e) => {
                        error_fmt!("Filter", "Failed to create path rewrite filter: {}", e);
                        Err(e)
                    }
                }
            },
            "rate_limit" => {
                let config: RateLimitFilterConfig = serde_json::from_value(config)
                    .map_err(|e| {
                        let err = ProxyError::FilterError(format!("Invalid rate limit filter config: {e}"));
                        error_fmt!("Filter", "{}", err);
                        err
                    })?;
                Ok(Arc::new(RateLimitFilter::new(config)))
            },
            "compression" => {
                let config: CompressionFilterConfig = serde_json::from_value(config)
                    .map_err(|e| {
                        let err = ProxyError::FilterError(format!("Invalid compression filter config: {e}"));
                        error_fmt!("Filter", "{}", err);
                        err
                    })?;
                Ok(Arc::new(CompressionFilter::new(config)))
            },
            "retry" => {
                let config: RetryFilterConfig = serde_json::from_value(config)
                    .map_err(|e| {
                        let err = ProxyError::FilterError(format!("Invalid retry filter config: {e}"));
                        error_fmt!("Filter", "{}", err);
                        err
                    })?;
                Ok(Arc::new(RetryFilter::new(config)))
            },
            "circuit_breaker" => {
                let config: CircuitBreakerFilterConfig = serde_json::from_value(config)
                    .map_err(|e| {
                        let err = ProxyError::FilterError(format!("Invalid circuit breaker filter config: {e}"));
                        error_fmt!("Filter", "{}", err);
                        err
                    })?;
                Ok(Arc::new(CircuitBreakerFilter::new(config)))
            },
            "enhanced_rate_limit" => {
                let config: EnhancedRateLimitFilterConfig = serde_json::from_value(config)
                    .map_err(|e| {
                        let err = ProxyError::FilterError(format!("Invalid enhanced rate limit filter config: {e}"));
                        error_fmt!("Filter", "{}", err);
                        err
                    })?;
                Ok(Arc::new(EnhancedRateLimitFilter::new(config)))
            },
            "caching" => {
                let config: CachingFilterConfig = serde_json::from_value(config)
                    .map_err(|e| {
                        let err = ProxyError::FilterError(format!("Invalid caching filter config: {e}"));
                        error_fmt!("Filter", "{}", err);
                        err
                    })?;
                Ok(Arc::new(CachingFilter::new(config)))
            },
            "auth" => {
                let config: AuthFilterConfig = serde_json::from_value(config)
                    .map_err(|e| {
                        let err = ProxyError::FilterError(format!("Invalid auth filter config: {e}"));
                        error_fmt!("Filter", "{}", err);
                        err
                    })?;
                Ok(Arc::new(AuthFilter::new(config)))
            },
            "cors" => {
                let config: CorsFilterConfig = serde_json::from_value(config)
                    .map_err(|e| {
                        let err = ProxyError::FilterError(format!("Invalid CORS filter config: {e}"));
                        error_fmt!("Filter", "{}", err);
                        err
                    })?;
                Ok(Arc::new(CorsFilter::new(config)))
            },
            "metrics" => {
                let config: MetricsFilterConfig = serde_json::from_value(config)
                    .map_err(|e| {
                        let err = ProxyError::FilterError(format!("Invalid metrics filter config: {e}"));
                        error_fmt!("Filter", "{}", err);
                        err
                    })?;
                match MetricsFilter::new(config) {
                    Ok(filter) => Ok(Arc::new(filter)),
                    Err(e) => {
                        error_fmt!("Filter", "Failed to create metrics filter: {}", e);
                        Err(e)
                    }
                }
            },
            "body_transform" => {
                let config: BodyTransformFilterConfig = serde_json::from_value(config)
                    .map_err(|e| {
                        let err = ProxyError::FilterError(format!("Invalid body transform filter config: {e}"));
                        error_fmt!("Filter", "{}", err);
                        err
                    })?;
                match BodyTransformFilter::new(config) {
                    Ok(filter) => Ok(Arc::new(filter)),
                    Err(e) => {
                        error_fmt!("Filter", "Failed to create body transform filter: {}", e);
                        Err(e)
                    }
                }
            },
            _ => {
                let err = ProxyError::FilterError(format!("Unknown filter type: {filter_type}"));
                error_fmt!("Filter", "{}", err);
                Err(err)
            },
        }
    }
}

// "request_sanitizer" => {
            //     let config: RequestSanitizerConfig = serde_json::from_value(config)
            //         .map_err(|e| ProxyError::FilterError(format!("Invalid request sanitizer config: {}", e)))?;
            //     Ok(Arc::new(RequestSanitizerFilter::new(config)))
            // },

// #[derive(Debug, Clone, Serialize, Deserialize)]
// pub struct RequestSanitizerConfig {
//     pub normalize_headers: bool,
//     pub validate_content_type: bool,
//     pub max_header_count: u32,
//     pub blocked_headers: Vec<String>,
// }

// #[derive(Debug)]
// pub struct RequestSanitizerFilter {
//     config: RequestSanitizerConfig,
// }

// impl RequestSanitizerFilter {
//     pub fn new(config: RequestSanitizerConfig) -> Self {
//         Self { config }
//     }
// }

// #[async_trait]
// impl Filter for RequestSanitizerFilter {
//     fn filter_type(&self) -> FilterType {
//         FilterType::Pre
//     }

//     fn name(&self) -> &str {
//         "request_sanitizer"
//     }

//     async fn pre_filter(
//         &self,
//         mut request: ProxyRequest,
//     ) -> Result<ProxyRequest, ProxyError> {
//         // Check header count
//         if request.headers.len() > self.config.max_header_count as usize {
//             return Err(ProxyError::FilterError("Header count exceeds maximum".to_string()));
//         }

//         // Normalize headers if enabled
//         if self.config.normalize_headers {
//             let mut new_headers = reqwest::header::HeaderMap::new();
//             for (name, value) in request.headers.iter() {
//                 let lower_name = name.as_str().to_lowercase();
//                 new_headers.insert(reqwest::header::HeaderName::from_lowercase(lower_name.as_bytes()).unwrap(), value.clone());
//             }
//             request.headers = new_headers;
//         }

//         // Validate content type if enabled
//         if self.config.validate_content_type {
//             if let Some(content_type) = request.headers.get("Content-Type") {
//                 let content_type_str = content_type.to_str().unwrap_or("");
//                 if !content_type_str.contains("/") {
//                     return Err(ProxyError::FilterError("Invalid Content-Type".to_string()));
//                 }
//             } else {
//                 return Err(ProxyError::FilterError("Missing Content-Type header".to_string()));
//             }
//         }

//         // Remove blocked headers
//         for header in &self.config.blocked_headers {
//             request.headers.remove(header);
//         }

//         Ok(request)
//     }
// }
